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
//! - **At terminal completion**, [`ColdReads::on_completion`] pops the
//!   device read and delivers each waiter a [`ColdDone`] carrying its
//!   subrange of the shared window. Every `ColdDone` drop releases that
//!   waiter's file pin; the **last** drop releases the buffer lease —
//!   exactly once across every cancellation interleaving (waiter
//!   resumed, cancelled before completion, cancelled after delivery,
//!   or a fully-cancelled merged read). Singleton and merged reads are
//!   one code path: a singleton is a window with one waiter.
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
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;

use inf_alloc::{AlignedBufId, AlignedPool};
use inf_foundation::{BuildIntHasher, LogHistogram};

use crate::driver::{CompletionResult, IoOp, RawFd, StableBytesMut};
use crate::gate::{GateWait, KeyedGate};
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

/// Why [`ColdReads::enqueue`] refused (backpressure, never failure).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub enum ColdRefused {
    /// The class FIFO is at `overflow_cap`. The command layer surfaces
    /// backpressure (retry next iteration / typed error upward). Pool
    /// dryness is no longer a refusal: it stalls the drain and counts
    /// `pool_dry` (S10 moved admission off the enqueue path).
    QueueFull,
}

/// A queued read intent — plain data; the §3.3 pin is taken at enqueue.
struct Intent {
    token: CompletionToken,
    fd: RawFd,
    file: TierFileId,
    offset: u64,
    len: u32,
    enqueued_us: u64,
}

/// One waiter's subrange of an in-flight device read.
struct Waiter {
    token: CompletionToken,
    /// Offset into the merged window.
    skip: u32,
    len: u32,
    enqueued_us: u64,
}

/// One in-flight device read (singleton or merged — same shape).
struct DeviceRead {
    buf: AlignedBufId,
    file: TierFileId,
    len: u32,
    waiters: Vec<Waiter>,
}

/// A terminally-completed window shared by its waiters' `ColdDone`s;
/// the buffer releases when `remaining` hits zero (last drop).
struct SharedWindow {
    buf: AlignedBufId,
    remaining: u32,
}

struct ColdState {
    pool: AlignedPool,
    config: ColdReadConfig,
    /// Bounded per-class FIFOs (index = [`ReadClass::index`]).
    pending: [VecDeque<Intent>; 2],
    /// Deficit grants remaining per class (refilled together when both
    /// are spent under contention).
    grants: [u32; 2],
    inflight: HashMap<CompletionToken, DeviceRead, BuildIntHasher>,
    windows: HashMap<CompletionToken, SharedWindow, BuildIntHasher>,
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
    /// Monotone read id → token {slot: low 24 bits, generation: high}.
    next_read: u64,
}

impl ColdState {
    fn unpin(pins: &mut HashMap<TierFileId, u32, BuildIntHasher>, file: TierFileId) {
        let count = pins.get_mut(&file).expect("pinned file has a pin entry");
        *count -= 1;
        if *count == 0 {
            pins.remove(&file);
        }
    }

    fn mint_token(&mut self) -> CompletionToken {
        let id = self.next_read;
        self.next_read += 1;
        CompletionToken::new(
            TokenClass::TierRead,
            (id & u64::from(MAX_SLOT)) as u32,
            (id >> 24) as u32,
        )
    }
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

/// The future a suspended cold read parks on; resolves to the custody-
/// carrying [`ColdDone`].
pub type ColdWait = GateWait<CompletionToken, ColdDone>;

/// One cell's cold-read path state: pool, pins, queues, in-flight table,
/// and the completion gate. Cheap-clone shared handle (cell-local `Rc`,
/// L1) — the plane's completion dispatch and every reading future share
/// it.
pub struct ColdReads {
    state: Rc<RefCell<ColdState>>,
    gate: KeyedGate<CompletionToken, ColdDone>,
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
    /// zero-limit config is a misconfiguration, not a throttle.
    #[must_use]
    pub fn with_config(pool: AlignedPool, config: ColdReadConfig) -> ColdReads {
        assert!(config.qd_cap > 0, "qd_cap must admit at least one device read");
        assert!(config.overflow_cap > 0, "overflow_cap must queue at least one intent");
        assert!(config.grants_foreground > 0, "foreground grants must be nonzero");
        assert!(config.grants_maintain > 0, "maintain grants must be nonzero");
        let grants = [config.grants_foreground, config.grants_maintain];
        ColdReads {
            state: Rc::new(RefCell::new(ColdState {
                pool,
                config,
                pending: [VecDeque::new(), VecDeque::new()],
                grants,
                inflight: HashMap::default(),
                windows: HashMap::default(),
                pins: HashMap::default(),
                counters: ColdReadCounters::default(),
                qd_hist: LogHistogram::new(),
                latency_hist: LogHistogram::new(),
                spare_waiters: Vec::new(),
                merge_scratch: Vec::new(),
                next_read: 0,
            })),
            gate: KeyedGate::new(),
        }
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
    /// in the queue/in-flight tables until the terminal completion
    /// routes through [`on_completion`](Self::on_completion).
    ///
    /// `now_us` is injected time (L7): the reactor clock in production,
    /// sim time in DST — it feeds the `cold_read_p99_us` histogram.
    ///
    /// # Errors
    /// [`ColdRefused::QueueFull`] when the class FIFO is at its bound —
    /// backpressure the caller shapes, never an error.
    ///
    /// # Panics
    /// Panics when `len` is zero or exceeds the pool buffer size — the
    /// caller computed the window from the record header, so a mismatch
    /// is a programmer error.
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
        assert!(len > 0, "empty cold read");
        assert!(len <= state.pool.buf_size(), "cold-read window exceeds a pool buffer");
        if state.pending[class.index()].len() >= state.config.overflow_cap {
            state.counters.queue_full += 1;
            return Err(ColdRefused::QueueFull);
        }
        let token = state.mint_token();
        *state.pins.entry(file).or_insert(0) += 1;
        state.pending[class.index()].push_back(Intent {
            token,
            fd,
            file,
            offset,
            len: len as u32,
            enqueued_us: now_us,
        });
        state.counters.enqueued += 1;
        let depth = (state.pending[0].len() + state.pending[1].len()) as u32;
        state.counters.queue_depth_high_water = state.counters.queue_depth_high_water.max(depth);
        drop(state);
        Ok(self.gate.waiter(token))
    }

    /// Admits queued intents to the device: merges (when enabled),
    /// leases buffers, and emits one `IoOp` per device read up to the
    /// QD cap under the class deficit. Called once per reactor iteration
    /// after the EXECUTE batch (and harmlessly more often — it is
    /// idempotent when nothing is admissible). Returns device reads
    /// issued.
    pub fn drain(&self, mut push: impl FnMut(IoOp)) -> u32 {
        let mut issued = 0u32;
        while let Some(op) = self.drain_one() {
            push(op);
            issued += 1;
        }
        issued
    }

    fn drain_one(&self) -> Option<IoOp> {
        let mut state_ref = self.state.borrow_mut();
        let state = &mut *state_ref;
        if state.inflight.len() >= state.config.qd_cap {
            return None;
        }
        Self::purge_stale_heads(state, &self.gate);
        if state.pending[0].is_empty() && state.pending[1].is_empty() {
            return None;
        }
        let Some(buf) = state.pool.try_lease() else {
            state.counters.pool_dry += 1;
            return None;
        };
        let class = Self::pick_class(state);
        Some(Self::admit(state, &self.gate, class, buf))
    }

    /// Drops cancelled-while-queued heads of both FIFOs (pins released,
    /// counted). Mid-queue stale intents are filtered by the merge scan
    /// or when they surface as heads — lazy, bounded, deterministic.
    fn purge_stale_heads(state: &mut ColdState, gate: &KeyedGate<CompletionToken, ColdDone>) {
        let ColdState { pending, pins, counters, .. } = state;
        for queue in pending.iter_mut() {
            while let Some(head) = queue.front() {
                if gate.has_waiter(&head.token) {
                    break;
                }
                let intent = queue.pop_front().expect("front observed");
                ColdState::unpin(pins, intent.file);
                counters.cancelled_queued += 1;
            }
        }
    }

    /// Deficit pick over non-empty classes: work-conserving when one
    /// class is idle (its grants are forfeit, not banked); 3:1 metering
    /// only under contention (ADR-0055 D3).
    fn pick_class(state: &mut ColdState) -> usize {
        let foreground = !state.pending[0].is_empty();
        let maintain = !state.pending[1].is_empty();
        debug_assert!(foreground || maintain, "caller checked non-empty");
        if !maintain {
            return 0;
        }
        if !foreground {
            return 1;
        }
        if state.grants[0] == 0 && state.grants[1] == 0 {
            state.grants = [state.config.grants_foreground, state.config.grants_maintain];
        }
        if state.grants[0] > 0 {
            state.grants[0] -= 1;
            0
        } else {
            state.grants[1] -= 1;
            1
        }
    }

    /// Pops the class FIFO's live head as the seed, joins mergeable
    /// intents (same file, adjacent/overlapping, union fits one pool
    /// buffer), and builds the device read. FIFO order of everything
    /// left behind is preserved (one rotate pass).
    fn admit(
        state: &mut ColdState,
        gate: &KeyedGate<CompletionToken, ColdDone>,
        class: usize,
        buf: AlignedBufId,
    ) -> IoOp {
        let buf_size = state.pool.buf_size();
        let merge = state.config.merge;
        {
            let ColdState { pending, pins, counters, merge_scratch, .. } = state;
            let queue = &mut pending[class];
            let seed = queue.pop_front().expect("picked class has a live head");
            debug_assert!(gate.has_waiter(&seed.token), "heads were purged");
            let mut lo = seed.offset;
            let mut hi = seed.offset + u64::from(seed.len);
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
                    if !gate.has_waiter(&intent.token) {
                        ColdState::unpin(pins, intent.file);
                        counters.cancelled_queued += 1;
                        continue;
                    }
                    let start = intent.offset;
                    let end = start + u64::from(intent.len);
                    let joins = intent.file == seed_file && start <= hi && end >= lo;
                    let new_lo = lo.min(start);
                    let new_hi = hi.max(end);
                    if joins && (new_hi - new_lo) as usize <= buf_size {
                        debug_assert_eq!(intent.fd, seed_fd, "one file, one fd");
                        lo = new_lo;
                        hi = new_hi;
                        merge_scratch.push(intent);
                    } else {
                        queue.push_back(intent);
                    }
                }
            }
        }
        let token = state.mint_token();
        let ColdState { pool, inflight, counters, qd_hist, spare_waiters, merge_scratch, .. } =
            state;
        let (fd, file) = (merge_scratch[0].fd, merge_scratch[0].file);
        let lo = merge_scratch.iter().map(|intent| intent.offset).min().expect("seed present");
        let hi = merge_scratch
            .iter()
            .map(|intent| intent.offset + u64::from(intent.len))
            .max()
            .expect("seed present");
        let window_len = (hi - lo) as usize;
        debug_assert!(window_len <= buf_size, "merge respected the buffer cap");
        let mut waiters = spare_waiters.pop().unwrap_or_default();
        debug_assert!(waiters.is_empty(), "spare lists come back drained");
        for intent in merge_scratch.drain(..) {
            waiters.push(Waiter {
                token: intent.token,
                skip: (intent.offset - lo) as u32,
                len: intent.len,
                enqueued_us: intent.enqueued_us,
            });
        }
        counters.issued += 1;
        counters.merged_waiters += waiters.len() as u64 - 1;
        let dest = &mut pool.bytes_mut(buf)[..window_len];
        // SAFETY: the pool buffer's address is stable for the pool's
        // lifetime (inf-alloc invariant) and its lease is held by the
        // in-flight table — released only when the last `ColdDone` built
        // at this device read's terminal completion drops — so nothing
        // reads, writes, or re-leases these bytes while the driver owns
        // them, regardless of what any issuing future does (including
        // being cancelled).
        let stable = unsafe { StableBytesMut::new(dest) };
        inflight.insert(token, DeviceRead { buf, file, len: window_len as u32, waiters });
        qd_hist.record(inflight.len() as u64);
        IoOp::TierRead { fd, offset: lo, buf: stable, token }
    }

    /// Routes a `TierRead`-class terminal completion: pops the device
    /// read, parks its window as shared custody, and delivers each
    /// waiter a [`ColdDone`] carrying its subrange. Returns how many
    /// live waiters received one; cancelled waiters' custody releases
    /// here (counted, the designed path — never a panic). `now_us`
    /// feeds the delivery-latency histogram (injected time, L7).
    ///
    /// # Panics
    /// Panics on a token this path never issued or already completed —
    /// duplicate/foreign terminal completions are driver-contract bugs.
    pub fn on_completion(
        &self,
        token: CompletionToken,
        result: CompletionResult,
        now_us: u64,
    ) -> u32 {
        let outcome = match result {
            CompletionResult::TierRead => Ok(()),
            CompletionResult::Error { errno, buf } => {
                debug_assert!(buf.is_none(), "tier reads never carry recv-pool buffers");
                Err(errno)
            }
            other => panic!("non-tier completion routed to ColdReads: {other:?}"),
        };
        let (file, mut waiters) = {
            let mut state = self.state.borrow_mut();
            match outcome {
                Ok(()) => state.counters.completed += 1,
                Err(_) => state.counters.errored += 1,
            }
            let read = state.inflight.remove(&token).expect("completion for an unknown cold read");
            for waiter in &read.waiters {
                debug_assert!(
                    waiter.skip + waiter.len <= read.len,
                    "every waiter subrange sits inside the window"
                );
            }
            state.windows.insert(
                token,
                SharedWindow { buf: read.buf, remaining: read.waiters.len() as u32 },
            );
            (read.file, read.waiters)
        };
        let mut delivered = 0u32;
        for waiter in waiters.drain(..) {
            let done = ColdDone {
                outcome,
                state: Rc::clone(&self.state),
                window: token,
                skip: waiter.skip,
                len: waiter.len,
                file,
            };
            if self.gate.complete(waiter.token, done) {
                delivered += 1;
                let mut state = self.state.borrow_mut();
                state.counters.delivered += 1;
                state.latency_hist.record(now_us.saturating_sub(waiter.enqueued_us));
            } else {
                self.state.borrow_mut().counters.unclaimed += 1;
            }
        }
        let mut state = self.state.borrow_mut();
        state.spare_waiters.push(waiters);
        delivered
    }

    /// Cold reads pinning `file` — queued, in flight, or delivered with
    /// live custody. The §3.3 pin observable: compaction/reclaim/drop
    /// defer close+unlink until this drains.
    #[must_use]
    pub fn inflight_on(&self, file: TierFileId) -> u32 {
        self.state.borrow().pins.get(&file).copied().unwrap_or(0)
    }

    /// Device reads in flight — `cold_reads_inflight`, bounded by the
    /// QD cap (merged reads count once; the S08-era hook, now capped).
    #[must_use]
    pub fn inflight_total(&self) -> usize {
        self.state.borrow().inflight.len()
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
/// window, the buffer lease: exactly once, wherever the values end up.
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
    /// `Ok` when the window is full ([`CompletionResult::TierRead`]);
    /// `Err(errno)` on the op's terminal error (EIO inside the flushed
    /// range is corruption — the caller surfaces it typed).
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
        assert_eq!(cold.on_completion(token, CompletionResult::TierRead, 5), 1);
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
        assert_eq!(cold.on_completion(token, CompletionResult::TierRead, 0), 0, "no live waiter");
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
        assert_eq!(cold.on_completion(token, CompletionResult::TierRead, 0), 1, "delivery parks");
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
        let result = CompletionResult::Error { errno: libc::EIO, buf: None };
        assert_eq!(cold.on_completion(token, result, 0), 1);
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
        cold.on_completion(token, CompletionResult::TierRead, 0);
        drop(block_on_ready(first)); // returns the lease
        let mut ops = drain_ops(&cold);
        assert_eq!(ops.len(), 1, "the queued intent proceeds after the release");
        let token = complete(ops.remove(0), 0xBB);
        cold.on_completion(token, CompletionResult::TierRead, 0);
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
                *offset
            })
            .collect();
        assert_eq!(issued, offsets[..2], "FIFO admission order");
        assert!(drain_ops(&cold).is_empty(), "cap holds while nothing completed");
        let token = complete(ops.into_iter().next().expect("first"), 0xCC);
        cold.on_completion(token, CompletionResult::TierRead, 0);
        let ops = drain_ops(&cold);
        assert_eq!(ops.len(), 1, "one freed slot admits one queued intent");
        let IoOp::TierRead { offset, .. } = &ops[0] else { panic!("TierRead") };
        assert_eq!(*offset, offsets[2], "oldest queued intent first");
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
        assert_eq!(offset, FRAME);
        assert_eq!(buf.len() as u64, 3 * FRAME, "the union window");
        let counters = cold.counters();
        assert_eq!(counters.issued, 1);
        assert_eq!(counters.merged_waiters, 3);
        let token = complete(ops.remove(0), 0xEE);
        assert_eq!(cold.on_completion(token, CompletionResult::TierRead, 9), 4, "full fan-out");
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
        assert_eq!(cold.on_completion(token, CompletionResult::TierRead, 0), 1);
        let counters = cold.counters();
        assert_eq!(counters.delivered, 1);
        assert_eq!(counters.unclaimed, 1, "the cancelled share released at delivery");
        let done = block_on_ready(keep);
        done.bytes(|bytes| assert!(bytes.iter().all(|&byte| byte == 0xDD)));
        drop(done);
        assert_eq!(cold.reconcile(), Ok(()));
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
            order.push(if offset >= 100 << 20 { 'm' } else { 'f' });
            let token = complete(ops.remove(0), 0x11);
            cold.on_completion(token, CompletionResult::TierRead, 0);
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
        assert_eq!(cold.on_completion(token, CompletionResult::TierRead, late_us + 12), 1);
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
        assert_eq!(cold.on_completion(token, CompletionResult::TierRead, 12), 1);
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

    /// Minimal single-future block_on for gate waiters whose value is
    /// already parked (first poll returns Ready — the gate contract).
    fn block_on_ready(waiter: ColdWait) -> ColdDone {
        use core::future::Future;
        use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
        fn noop(_: *const ()) {}
        fn clone(p: *const ()) -> RawWaker {
            RawWaker::new(p, &VTABLE)
        }
        static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, noop, noop, noop);
        // SAFETY: the no-op waker never dereferences its pointer.
        let waker = unsafe { Waker::from_raw(RawWaker::new(core::ptr::null(), &VTABLE)) };
        let mut waiter = waiter;
        match core::pin::Pin::new(&mut waiter).poll(&mut Context::from_waker(&waker)) {
            Poll::Ready(done) => done,
            Poll::Pending => panic!("value was parked; first poll must be Ready"),
        }
    }
}
