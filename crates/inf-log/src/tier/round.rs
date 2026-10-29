//! A reactor-drive flush round's holders (ADR-0084 D2/D3, ADR-0155 D2/D3):
//! the staged ops with their states and positions, the deferred effects,
//! the window pool the round's writes take from, and the positions its
//! plan parsed. Every holder is capped and built with the pipeline, so a
//! round allocates nothing.

use std::rc::Rc;

use inf_foundation::FileOffset;
use inf_foundation::bounded::{CapCensus, CapError, CappedDeque, Reserve, Reserved};

use super::{FrameStaging, TIER_BATCH_FRAMES, TIER_BATCH_PAYLOAD_BYTES, TIER_FRAME_BYTES};
use crate::limits::{
    TIER_POOL_BATCH_WINDOWS, TIER_POOL_BATCH_WINDOWS_MAX, TIER_POOL_SINGLE_WINDOWS,
    TIER_POOL_SINGLE_WINDOWS_CAP, TIER_ROUND_BARRIERS, TIER_ROUND_EFFECTS, TIER_ROUND_EFFECTS_MAX,
    TIER_ROUND_WRITES, TIER_ROUND_WRITES_MAX,
};

/// One staged op of a reactor-drive flush round (M4.5-S31, ADR-0084):
/// the plane converts writes to `IoOp::LogWrite` and barriers to
/// `IoOp::Fdatasync`. `bytes` borrows the pool window the op's write holds
/// — heap-stable until the round finishes (the driver's `StableBytes`
/// custody proof). The position is the one the round's plan parsed
/// (ADR-0167 A1), so no consumer parses a tier-round position.
pub struct TierOpView<'a> {
    /// Backend fd the op targets.
    pub fd: std::os::fd::RawFd,
    /// `true` = fdatasync barrier (`bytes` is empty).
    pub is_barrier: bool,
    /// Absolute file position of a write; a barrier has none and carries
    /// position 0, which nothing reads.
    pub offset: FileOffset,
    /// The aligned window prefix to write (empty for barriers).
    pub bytes: &'a [u8],
    /// Where the op stands between its plan and its round's end.
    pub state: OpState,
}

/// One staged op's state (ADR-0084 D2, ADR-0178 T4): kept in the op
/// itself, so no second table beside the round's ops exists to keep in
/// step with them. An op is sent only from `Unsent`, completes only from
/// `Sent`, and a failed write is made `Unsent` again, byte-identical, for
/// its retry.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum OpState {
    /// Staged, not yet handed to the driver.
    Unsent,
    /// Handed to the driver; its completion has not arrived.
    Sent,
    /// Completed successfully.
    Done,
    /// Completed with an error: a write is retried, a barrier is fatal.
    Failed,
}

/// A deferred durability fact of one flush round (ADR-0084 D2) —
/// applied in stage order at the round's last barrier completion,
/// never at submission: a watermark moves only once the barrier that
/// covers its bytes has completed. The store's
/// `complete_flush_round` is the single applier.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RoundEffect {
    /// The active writer's `durable_len` advances to `data_len`.
    DurableTo {
        /// Data bytes the round's barrier covers.
        data_len: u64,
    },
    /// The oldest pending seal commits to the catalog.
    SealCommit,
    /// `flushed` may cross the ADR-0052 D2 gap ending at `to` (the
    /// covering seal's barrier is in the same round, earlier in order).
    GapCross {
        /// First address past the sealed-dead interval.
        to: u64,
    },
}

/// One staged write held by a round until its terminal completion: its
/// window stays the round's, by the pool's token, until the round ends.
struct StagedWrite {
    fd: std::os::fd::RawFd,
    offset: FileOffset,
    window: PooledWindow,
    frames: usize,
    state: OpState,
}

/// One staged fdatasync barrier.
struct StagedBarrier {
    fd: std::os::fd::RawFd,
    state: OpState,
}

/// The staged I/O of one reactor-drive flush round (ADR-0084 D2/D3,
/// ADR-0155 D3): writes, barriers and deferred effects in capped holders
/// built once with the pipeline and emptied at each round's end, so a
/// round allocates nothing. Ops are viewed by index (writes first, then
/// barriers) so the plane can resubmit identical intents after a
/// write-completion error (the retained-batch retry at the reactor tier).
/// The reactor drive requires fd-backed files — fd-less filesystems
/// (`MemFs`) stay on the seam drive by construction.
pub struct TierRound {
    writes: CappedDeque<StagedWrite, Reserve>,
    barriers: CappedDeque<StagedBarrier, Reserve>,
    effects: CappedDeque<RoundEffect, Reserve>,
}

impl TierRound {
    /// The round's holders, one row each on `census`: at most
    /// `TIER_ROUND_WRITES_MAX`, `…_BARRIERS_MAX` and `…_EFFECTS_MAX`.
    pub(crate) fn new(census: &Rc<CapCensus>) -> Result<TierRound, CapError> {
        Ok(TierRound {
            writes: CappedDeque::new(TIER_ROUND_WRITES, census)?,
            barriers: CappedDeque::new(TIER_ROUND_BARRIERS, census)?,
            effects: CappedDeque::new(TIER_ROUND_EFFECTS, census)?,
        })
    }

    /// Whether a round is staged: every plan stages at least one effect
    /// (a `DurableTo`, a `SealCommit` or a `GapCross`), and the effects
    /// are taken only when the round ends.
    #[must_use]
    pub fn is_staged(&self) -> bool {
        !self.effects.is_empty()
    }

    /// Ops staged for the driver (writes first, barriers after — the
    /// plane submits barriers only once every write completed).
    #[must_use]
    pub fn op_count(&self) -> usize {
        self.writes.len() + self.barriers.len()
    }

    /// Leading ops that are data writes.
    #[must_use]
    pub fn write_count(&self) -> usize {
        self.writes.len()
    }

    /// Barrier ops in this round — nonzero exactly when the round stages
    /// any durability fact on a file.
    #[must_use]
    pub fn barrier_count(&self) -> usize {
        self.barriers.len()
    }

    /// The op at `index` (stage order: writes, then barriers), its write's
    /// bytes read from `pool`; `None` past `op_count`.
    pub(crate) fn op<'a>(&self, index: usize, pool: &'a WindowPool) -> Option<TierOpView<'a>> {
        if let Some(w) = self.writes.get(index) {
            return Some(TierOpView {
                fd: w.fd,
                is_barrier: false,
                offset: w.offset,
                bytes: pool.staging(&w.window).filled(w.frames),
                state: w.state,
            });
        }
        let barrier = self.barriers.get(index.checked_sub(self.writes.len())?)?;
        Some(TierOpView {
            fd: barrier.fd,
            is_barrier: true,
            offset: FileOffset::from_u32_bytes(0),
            bytes: &[],
            state: barrier.state,
        })
    }

    /// The state of op `index`, mutably; `None` past `op_count`.
    fn state_mut(&mut self, index: usize) -> Option<&mut OpState> {
        let writes = self.writes.len();
        match index.checked_sub(writes) {
            None => self.writes.get_mut(index).map(|w| &mut w.state),
            Some(barrier) => self.barriers.get_mut(barrier).map(|b| &mut b.state),
        }
    }

    /// Moves op `index` from `from` to `to`; `false` (nothing changed)
    /// when the op is absent or not in `from`.
    pub(crate) fn transition(&mut self, index: usize, from: OpState, to: OpState) -> bool {
        match self.state_mut(index) {
            Some(state) if *state == from => {
                *state = to;
                true
            }
            Some(_) | None => false,
        }
    }

    /// Makes every failed write unsent again, byte-identical, for its
    /// retry; returns how many.
    pub(crate) fn requeue_failed_writes(&mut self) -> usize {
        let mut requeued = 0;
        for index in 0..self.writes.len() {
            if let Some(write) = self.writes.get_mut(index)
                && write.state == OpState::Failed
            {
                write.state = OpState::Unsent;
                requeued += 1;
            }
        }
        requeued
    }

    /// Whether every write has been handed to the driver at least once
    /// this attempt (none is `Unsent`).
    pub(crate) fn writes_all_sent(&self) -> bool {
        self.writes.iter().all(|w| w.state != OpState::Unsent)
    }

    /// Appends a write the plan reserved: its position, the window token
    /// that holds its bytes and the frames it carries.
    ///
    /// # Panics
    /// Panics past `TIER_ROUND_WRITES_MAX`: the plan bounds a round's
    /// writes at `⌈A/Q⌉ + 5` with `A` at most the pool's span bound.
    pub(crate) fn push_write(
        &mut self,
        fd: std::os::fd::RawFd,
        offset: FileOffset,
        window: PooledWindow,
        frames: usize,
    ) {
        let write = StagedWrite { fd, offset, window, frames, state: OpState::Unsent };
        match self.writes.reserve() {
            Reserved::Slot(slot) => slot.publish(write),
            Reserved::Full => panic!("a tier round write past the round plan's bound"),
        }
    }

    /// Appends a barrier the plan reserved.
    ///
    /// # Panics
    /// Panics past `TIER_ROUND_BARRIERS_MAX`: a plan stages at most one
    /// old seal, two directory barriers and one sync or seal.
    pub(crate) fn push_barrier(&mut self, fd: std::os::fd::RawFd) {
        match self.barriers.reserve() {
            Reserved::Slot(slot) => slot.publish(StagedBarrier { fd, state: OpState::Unsent }),
            Reserved::Full => panic!("a tier round barrier past the round plan's bound"),
        }
    }

    /// Appends a deferred effect the plan reserved.
    ///
    /// # Panics
    /// Panics past `TIER_ROUND_EFFECTS_MAX`: a plan stages at most a
    /// rotation's seal, then a gap's seal and crossing or one sync.
    pub(crate) fn push_effect(&mut self, effect: RoundEffect) {
        match self.effects.reserve() {
            Reserved::Slot(slot) => slot.publish(effect),
            Reserved::Full => panic!("a tier round effect past the round plan's bound"),
        }
    }

    /// Ends the round: its windows return to `pool`, its holders empty,
    /// and its deferred effects come back in stage order. Called once
    /// every op is terminal — never while the driver may still touch a
    /// window. At most `TIER_ROUND_EFFECTS_MAX` effects, no allocation.
    pub(crate) fn finish(&mut self, pool: &mut WindowPool) -> RoundEffects {
        while let Some(write) = self.writes.pop_front() {
            pool.put(write.window);
        }
        while self.barriers.pop_front().is_some() {}
        pool.release_reservation();
        let mut effects = RoundEffects::default();
        while let Some(effect) = self.effects.pop_front() {
            effects.push(effect);
        }
        effects
    }
}

/// A finished round's effects in stage order: at most
/// `TIER_ROUND_EFFECTS_MAX`, held inline (no allocation per round).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct RoundEffects {
    list: [Option<RoundEffect>; TIER_ROUND_EFFECTS_MAX as usize],
    len: usize,
    next: usize,
}

impl RoundEffects {
    fn push(&mut self, effect: RoundEffect) {
        debug_assert!(self.len < self.list.len(), "a round's effects fit their cap");
        if let Some(slot) = self.list.get_mut(self.len) {
            *slot = Some(effect);
            self.len += 1;
        }
    }
}

impl Iterator for RoundEffects {
    type Item = RoundEffect;

    fn next(&mut self) -> Option<RoundEffect> {
        if self.next >= self.len {
            return None;
        }
        let effect = self.list.get(self.next).copied().flatten();
        self.next += 1;
        effect
    }
}

/// The two window sizes a pool holds: a batch of `TIER_BATCH_FRAMES`
/// frames (one ≥ 1 MiB device write) or one block (a header, a footer, a
/// partial tail frame).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub(crate) enum WindowKind {
    Batch,
    Single,
}

/// The reactor drive's window pool (ADR-0155 D2/D3, L5): built with the
/// pipeline, `⌈max(S, M)/Q⌉` batch windows and five single-block
/// windows, `S` the slice, `M` the largest record the namespace admits
/// and `Q` a batch's payload bytes. Each window's capacity is reserved at
/// the build and written (zeroed) only at its first take, so a window
/// never taken adds no resident page; the lists are taken LIFO from the
/// front of each holder, so the windows ever written number the most out
/// at once. A plan reserves the windows its round takes before any writer
/// effect, and nothing is allocated between the pool's build and its
/// drop. One round per pipeline, so the reservation is one value.
pub(crate) struct WindowPool {
    batch: CappedDeque<FrameStaging, Reserve>,
    single: CappedDeque<FrameStaging, Reserve>,
    /// Windows out: the first `batch_out` and `single_out` of each holder.
    batch_out: usize,
    single_out: usize,
    /// Windows the staged round's plan reserved and has not yet taken.
    reserved: WindowNeeds,
    /// Bytes of the windows written at least once (the LIFO high water).
    written_bytes: u64,
}

/// The windows one plan needs, by kind.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct WindowNeeds {
    pub(crate) batch: usize,
    pub(crate) single: usize,
}

/// A window the pool issued: an index into its kind's holder,
/// made by [`WindowPool::take`] alone and neither `Copy` nor `Clone`, so
/// a round holds each issued window exactly once and returns it once.
pub(crate) struct PooledWindow {
    kind: WindowKind,
    index: usize,
}

impl WindowPool {
    /// The pool of a pipeline whose span is at most `span_bytes_max`
    /// record bytes: `⌈span_bytes_max/Q⌉` batch windows and
    /// `TIER_POOL_SINGLE_WINDOWS` single ones, each a census row's entry,
    /// capacity reserved and nothing written. A span bound above the
    /// pool's cap is `CapError::Layout`; nothing is built on a refusal.
    pub(crate) fn new(span_bytes_max: u64, census: &Rc<CapCensus>) -> Result<WindowPool, CapError> {
        let batches = span_bytes_max.div_ceil(TIER_BATCH_PAYLOAD_BYTES);
        let batches = usize::try_from(batches).map_err(|_| CapError::Layout)?;
        if batches > TIER_POOL_BATCH_WINDOWS_MAX as usize {
            return Err(CapError::Layout);
        }
        let mut batch = CappedDeque::new(TIER_POOL_BATCH_WINDOWS, census)?;
        let mut single = CappedDeque::new(TIER_POOL_SINGLE_WINDOWS_CAP, census)?;
        for (holder, count, frames) in [
            (&mut batch, batches, TIER_BATCH_FRAMES),
            (&mut single, TIER_POOL_SINGLE_WINDOWS as usize, 1),
        ] {
            for _ in 0..count {
                let window = FrameStaging::reserved(frames)?;
                match holder.reserve() {
                    Reserved::Slot(slot) => slot.publish(window),
                    Reserved::Full => return Err(CapError::Layout),
                }
            }
        }
        Ok(WindowPool {
            batch,
            single,
            batch_out: 0,
            single_out: 0,
            reserved: WindowNeeds::default(),
            written_bytes: 0,
        })
    }

    /// Record bytes the largest span this pool serves may hold: whole
    /// batch windows' payload.
    pub(crate) fn span_bytes_max(&self) -> u64 {
        self.batch.len() as u64 * TIER_BATCH_PAYLOAD_BYTES
    }

    /// Reserves `needs` for one round's plan: `true` when both kinds have
    /// that many windows free and no round holds a reservation; `false`
    /// changes nothing.
    pub(crate) fn reserve(&mut self, needs: WindowNeeds) -> bool {
        let idle = self.batch_out == 0 && self.single_out == 0;
        let none_held = self.reserved == WindowNeeds::default();
        let fits = needs.batch <= self.batch.len() && needs.single <= self.single.len();
        if !(idle && none_held && fits) {
            return false;
        }
        self.reserved = needs;
        true
    }

    /// Drops a reservation no window was taken from (a plan refused after
    /// it reserved).
    pub(crate) fn release_reservation(&mut self) {
        self.reserved = WindowNeeds::default();
    }

    /// Takes the next window of `kind` from the round's reservation,
    /// zeroing it on its first take.
    ///
    /// # Panics
    /// Panics when the plan reserved no further window of the kind: a
    /// plan reserves exactly the windows its stage takes.
    pub(crate) fn take(&mut self, kind: WindowKind) -> PooledWindow {
        let (holder, out, left) = match kind {
            WindowKind::Batch => (&mut self.batch, &mut self.batch_out, &mut self.reserved.batch),
            WindowKind::Single => {
                (&mut self.single, &mut self.single_out, &mut self.reserved.single)
            }
        };
        assert!(*left > 0, "a tier window past the round plan's reservation");
        *left -= 1;
        let index = *out;
        let window = holder.get_mut(index).expect("a reservation is within the pool");
        if window.zero_once() {
            self.written_bytes += window.capacity_bytes();
        }
        *out += 1;
        PooledWindow { kind, index }
    }

    /// Returns a window the round held. One round holds every window out
    /// and returns them all at its end, so the LIFO top falls back to the
    /// holder's front and the next round takes the same windows first.
    ///
    /// # Panics
    /// Panics on a return below none out: a token is made once per take
    /// and consumed once (release-checked: a floor a debug assert alone
    /// would let wrap).
    pub(super) fn put(&mut self, window: PooledWindow) {
        let PooledWindow { kind, index } = window;
        let out = match kind {
            WindowKind::Batch => &mut self.batch_out,
            WindowKind::Single => &mut self.single_out,
        };
        debug_assert!(index < TIER_ROUND_WRITES_MAX as usize, "a window of the pool");
        *out = out.checked_sub(1).expect("window returned twice");
    }

    /// The window a token names.
    pub(crate) fn staging(&self, window: &PooledWindow) -> &FrameStaging {
        let holder = match window.kind {
            WindowKind::Batch => &self.batch,
            WindowKind::Single => &self.single,
        };
        holder.get(window.index).expect("a pooled window names its pool's entry")
    }

    /// The window a token names, writable.
    pub(crate) fn staging_mut(&mut self, window: &PooledWindow) -> &mut FrameStaging {
        let holder = match window.kind {
            WindowKind::Batch => &mut self.batch,
            WindowKind::Single => &mut self.single,
        };
        holder.get_mut(window.index).expect("a pooled window names its pool's entry")
    }

    /// The planted early return (`inf_canary_tier_window_early_put`): every
    /// window goes back to the free lists while the round still holds its
    /// tokens. Compiled into every build, reached only by the plant.
    pub(crate) fn recycle_early(&mut self) {
        self.batch_out = 0;
        self.single_out = 0;
    }

    /// Writes `byte` over every window the pool holds free and has
    /// written before — the pool's next user, as a custody test sees it.
    #[cfg(test)]
    pub(crate) fn overwrite_free(&mut self, byte: u8) {
        for (holder, out) in
            [(&mut self.batch, self.batch_out), (&mut self.single, self.single_out)]
        {
            for index in out..holder.len() {
                if let Some(window) = holder.get_mut(index) {
                    window.raw.fill(byte);
                }
            }
        }
    }

    /// Windows out of the pool now (tests: the count is exact after every
    /// round).
    #[cfg(test)]
    pub(crate) fn outstanding(&self) -> usize {
        self.batch_out + self.single_out
    }

    /// The bytes the pool reserved at its build: every window's capacity.
    pub(crate) fn capacity_bytes(&self) -> u64 {
        self.batch.iter().chain(self.single.iter()).map(FrameStaging::capacity_bytes).sum()
    }

    /// The bytes of the windows written at least once.
    pub(crate) fn written_bytes(&self) -> u64 {
        self.written_bytes
    }
}

/// The write positions a round's plan parsed (ADR-0167 A1), in the order
/// its stage pushes the writes: each staged write takes the next, so a
/// tier-round position is parsed once, in the round plan, and never again. At
/// most `TIER_ROUND_WRITES_MAX`, held inline.
pub(crate) struct PlannedWrites {
    list: [FileOffset; TIER_ROUND_WRITES_MAX as usize],
    len: usize,
    next: usize,
}

impl PlannedWrites {
    pub(crate) fn new() -> PlannedWrites {
        PlannedWrites {
            list: [FileOffset::from_u32_bytes(0); TIER_ROUND_WRITES_MAX as usize],
            len: 0,
            next: 0,
        }
    }

    /// Appends a parsed position; the plan pushes at most its writes,
    /// which the round's write cap bounds.
    pub(crate) fn push(&mut self, position: FileOffset) {
        debug_assert!(self.len < self.list.len(), "a plan's writes fit the round's cap");
        if let Some(slot) = self.list.get_mut(self.len) {
            *slot = position;
            self.len += 1;
        }
    }

    /// The next planned position. `expected` is the writer's own reckoning
    /// of it: the plan and the stage walk one layout, paired at every write
    /// (with [`is_spent`](Self::is_spent) at the stage's end, a writer that
    /// pushes more, fewer or other writes than the plan listed stops the
    /// cell before any op is issued, never writing a frame at a position
    /// meant for another).
    ///
    /// # Panics
    /// Panics past the round plan's writes, or on a position the writer
    /// reckons differently: the plan lists every write its stage pushes,
    /// in order.
    pub(crate) fn next(&mut self, expected: u64) -> FileOffset {
        let position = self.list[..self.len]
            .get(self.next)
            .copied()
            .expect("a staged tier write the plan did not list");
        assert_eq!(position.bytes(), expected, "the round plan's position is the stage's");
        self.next += 1;
        position
    }

    /// Whether the stage took every planned position.
    pub(crate) fn is_spent(&self) -> bool {
        self.next == self.len
    }
}

impl FrameStaging {
    /// A window whose capacity is reserved and not written: no byte is
    /// readable until [`zero_once`](Self::zero_once), so a window never
    /// taken adds no resident page. The alignment is fixed now, from the
    /// allocation, which the zeroing fills without moving.
    pub(crate) fn reserved(frames: usize) -> Result<FrameStaging, CapError> {
        let bytes = frames
            .checked_add(1)
            .and_then(|slots| slots.checked_mul(TIER_FRAME_BYTES))
            .ok_or(CapError::Layout)?;
        let mut raw: Vec<u8> = Vec::new();
        raw.try_reserve_exact(bytes).map_err(|_| CapError::Alloc)?;
        let at = raw.as_ptr().align_offset(TIER_FRAME_BYTES);
        if at >= TIER_FRAME_BYTES {
            return Err(CapError::Layout);
        }
        Ok(FrameStaging { raw, at, frames })
    }

    /// Zeroes a reserved window the first time it is taken, inside its
    /// reserved capacity (no allocation); `true` when this call wrote it.
    pub(crate) fn zero_once(&mut self) -> bool {
        if !self.raw.is_empty() {
            return false;
        }
        let before = self.raw.as_ptr();
        self.raw.resize(self.capacity_bytes_usize(), 0);
        debug_assert_eq!(before, self.raw.as_ptr(), "zeroing fills the reserved capacity");
        true
    }

    /// The bytes this window holds once written: its frames and the one
    /// frame of alignment slack.
    pub(crate) fn capacity_bytes(&self) -> u64 {
        self.capacity_bytes_usize() as u64
    }

    fn capacity_bytes_usize(&self) -> usize {
        TIER_FRAME_BYTES * (self.frames + 1)
    }
}
