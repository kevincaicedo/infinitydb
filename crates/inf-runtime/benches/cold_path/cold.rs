//! Leg A: the cold-read host as it stood before its fixed stores, copied
//! whole so every A leg measures the same code after the engine's host
//! changes. Its tables are std's: per-class `VecDeque` intent queues, a
//! `HashMap` of device reads in flight, one of completed windows and one of
//! per-file pin counts, a `Vec` of waiters per device read; a receipt
//! delivers every member of its window before it returns, and a drain
//! issues to the device cap, each seed rescanning its class's queue.

use core::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;

use inf_alloc::{AlignedBufId, AlignedPool};
use inf_foundation::limits::DRIVER_OP_BYTES_MAX;
use inf_foundation::{BuildIntHasher, FileOffset, FileOffsetRefused, LogHistogram};

use crate::gate::{GateWait, KeyedGate};
use inf_runtime::{
    CompletionResult, CompletionToken, IoOp, Issue, MAX_SLOT, RawFd, StableBytesMut, TokenClass,
};

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

#[derive(Copy, Clone, Debug)]
pub struct ColdReadConfig {
    pub qd_cap: usize,
    pub overflow_cap: usize,
    pub grants_foreground: u32,
    pub grants_maintain: u32,
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

#[derive(Copy, Clone, Default, Debug, PartialEq, Eq)]
pub struct ColdReadCounters {
    pub enqueued: u64,
    pub issued: u64,
    pub merged_waiters: u64,
    pub completed: u64,
    pub errored: u64,
    pub delivered: u64,
    pub unclaimed: u64,
    pub cancelled_queued: u64,
    pub pool_dry: u64,
    pub queue_full: u64,
    pub queue_depth_high_water: u32,
    pub maintain_deferred: u64,
}

impl ColdReadCounters {
    #[must_use]
    pub fn coalesce_ratio_milli(&self) -> u64 {
        let saved = self.enqueued.saturating_sub(self.issued);
        saved.saturating_mul(1000) / self.enqueued.max(1)
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum ColdRefused {
    QueueFull,
    Unrepresentable(UnrepresentableRead),
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum UnrepresentableRead {
    OffsetAboveMax(FileOffsetRefused),
    EmptyWindow,
    WindowAboveMax { window_bytes: usize, window_bytes_max: usize },
}

struct Intent {
    token: CompletionToken,
    fd: RawFd,
    file: TierFileId,
    offset: FileOffset,
    len: u32,
    enqueued_us: u64,
}

struct Waiter {
    token: CompletionToken,
    skip: u32,
    len: u32,
    enqueued_us: u64,
}

struct DeviceRead {
    buf: AlignedBufId,
    file: TierFileId,
    len: u32,
    waiters: Vec<Waiter>,
}

struct SharedWindow {
    buf: AlignedBufId,
    remaining: u32,
}

struct ColdState {
    pool: AlignedPool,
    config: ColdReadConfig,
    pending: [VecDeque<Intent>; 2],
    grants: [u32; 2],
    inflight: HashMap<CompletionToken, DeviceRead, BuildIntHasher>,
    windows: HashMap<CompletionToken, SharedWindow, BuildIntHasher>,
    pins: HashMap<TierFileId, u32, BuildIntHasher>,
    counters: ColdReadCounters,
    qd_hist: LogHistogram,
    latency_hist: LogHistogram,
    spare_waiters: Vec<Vec<Waiter>>,
    merge_scratch: Vec<Intent>,
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
    let len = u32::try_from(window_bytes).map_err(|_| too_wide)?;
    Ok((offset, len))
}

#[derive(Debug, PartialEq, Eq)]
pub struct ColdLeak {
    pub leased: usize,
    pub inflight: usize,
    pub pinned_files: usize,
    pub queued: usize,
    pub windows: usize,
}

pub type ColdWait = GateWait<CompletionToken, ColdDone>;

pub struct ColdReads {
    state: Rc<RefCell<ColdState>>,
    gate: KeyedGate<CompletionToken, ColdDone>,
}

impl ColdReads {
    #[must_use]
    pub fn new(pool: AlignedPool) -> ColdReads {
        ColdReads::with_config(pool, ColdReadConfig::default())
    }

    #[must_use]
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

    #[must_use]
    pub fn buf_size(&self) -> usize {
        self.state.borrow().pool.buf_size()
    }

    #[must_use]
    pub fn pool_reserved_bytes(&self) -> u64 {
        self.state.borrow().pool.reserved_bytes()
    }

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
            len,
            enqueued_us: now_us,
        });
        state.counters.enqueued += 1;
        let depth = (state.pending[0].len() + state.pending[1].len()) as u32;
        state.counters.queue_depth_high_water = state.counters.queue_depth_high_water.max(depth);
        drop(state);
        Ok(self.gate.waiter(token))
    }

    pub fn drain(&self, push: impl FnMut(IoOp)) -> u32 {
        self.drain_budgeted(|_, _| Issue::Now, |_, _, _| {}, push)
    }

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
            if state.inflight.len() >= state.config.qd_cap {
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
            let mut lo = seed.offset.bytes();
            let mut hi = seed.offset.bytes_after(seed.len);
            let (seed_fd, seed_file) = (seed.fd, seed.file);
            debug_assert!(merge_scratch.is_empty(), "scratch drained after every admit");
            merge_scratch.push(seed);
            if merge {
                for _ in 0..queue.len() {
                    let intent = queue.pop_front().expect("counted");
                    if !gate.has_waiter(&intent.token) {
                        ColdState::unpin(pins, intent.file);
                        counters.cancelled_queued += 1;
                        continue;
                    }
                    let start = intent.offset.bytes();
                    let end = intent.offset.bytes_after(intent.len);
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
        }
        let token = state.mint_token();
        let ColdState { pool, inflight, counters, qd_hist, spare_waiters, merge_scratch, .. } =
            state;
        let (fd, file) = (merge_scratch[0].fd, merge_scratch[0].file);
        let lo = merge_scratch.iter().map(|intent| intent.offset).min().expect("seed present");
        let hi = merge_scratch
            .iter()
            .map(|intent| intent.offset.bytes_after(intent.len))
            .max()
            .expect("seed present");
        let window_len = (hi - lo.bytes()) as usize;
        debug_assert!(window_len <= buf_size, "merge respected the buffer cap");
        let mut waiters = spare_waiters.pop().unwrap_or_default();
        debug_assert!(waiters.is_empty(), "spare lists come back drained");
        for intent in merge_scratch.drain(..) {
            waiters.push(Waiter {
                token: intent.token,
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
        // in-flight table — released only when the last `ColdDone` built
        // at this device read's terminal completion drops — so nothing
        // reads, writes, or re-leases these bytes while the driver owns
        // them, regardless of what any issuing future does (including
        // being cancelled).
        let stable = unsafe { StableBytesMut::new(target) };
        inflight.insert(token, DeviceRead { buf, file, len: window_len as u32, waiters });
        qd_hist.record(inflight.len() as u64);
        IoOp::TierRead { fd, offset: lo, buf: stable, token }
    }

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
            other @ (CompletionResult::Accepted { .. }
            | CompletionResult::Recv { .. }
            | CompletionResult::RecvDropped
            | CompletionResult::Sent { .. }
            | CompletionResult::LogWritten
            | CompletionResult::Synced
            | CompletionResult::Closed) => {
                panic!("non-tier completion routed to ColdReads: {other:?}")
            }
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

    #[must_use]
    pub fn inflight_on(&self, file: TierFileId) -> u32 {
        self.state.borrow().pins.get(&file).copied().unwrap_or(0)
    }

    #[must_use]
    pub fn inflight_total(&self) -> usize {
        self.state.borrow().inflight.len()
    }

    #[must_use]
    pub fn queue_depth(&self) -> usize {
        let state = self.state.borrow();
        state.pending[0].len() + state.pending[1].len()
    }

    #[must_use]
    pub fn qd_percentile(&self, p: f64) -> u64 {
        self.state.borrow().qd_hist.percentile(p)
    }

    #[must_use]
    pub fn latency_percentile_us(&self, p: f64) -> u64 {
        self.state.borrow().latency_hist.percentile(p)
    }

    #[must_use]
    pub fn counters(&self) -> ColdReadCounters {
        self.state.borrow().counters
    }

    #[must_use]
    pub fn qd_cap(&self) -> usize {
        self.state.borrow().config.qd_cap
    }

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

pub struct ColdDone {
    outcome: Result<(), i32>,
    state: Rc<RefCell<ColdState>>,
    window: CompletionToken,
    skip: u32,
    len: u32,
    file: TierFileId,
}

impl ColdDone {
    pub fn outcome(&self) -> Result<(), i32> {
        self.outcome
    }

    #[must_use]
    pub fn file(&self) -> TierFileId {
        self.file
    }

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
