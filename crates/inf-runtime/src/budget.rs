//! Per-cell device budget (M4.5-S36, ADR-0088 D1/D2/D2b): device I/O
//! classes over a measured device model, spent through per-class byte
//! and op deficits refilled on the injected clock.
//!
//! The shape is Seastar's io-queue with the one change L1 demands:
//! shares are **static per cell** (the device model is divided by the
//! cell count at boot; nothing here is shared between cells), so the
//! structure is a plain owned value the plane refills once per MAINTAIN
//! entry from `LoopCx::now` and consults at the four background issuing
//! sites. Foreground classes are *metered* (charged, so the background
//! grant is work-conserving toward the log) and **never deferred**; a
//! background class whose deficit cannot cover the slice it wants is
//! told `Deferred` — "not this slice" — and its own state machine
//! re-offers next tick. Nothing queues, nothing allocates, nothing waits.
//!
//! Refill arithmetic (per direction — write, read — all integers,
//! saturating): the grant for the elapsed interval is the cell's share
//! of the modeled rate minus what the foreground spent since the last
//! refill, clamped at no less than one [`FLOOR_DIVISOR`]th of the share
//! (a background that cannot run at all is a recovery-time bug, a
//! foreground stall, or a class downgrade — under foreground saturation
//! the device is shared 7:1, visibly). The grant is split among the
//! background classes by weight; each class's deficit is **capped** at
//! `max(slice, share × weight/Σw × BURST_HORIZON)` and what a capped
//! class cannot hold flows to a per-direction shared pool (itself
//! capped at `share × BURST_HORIZON`) any class may draw after its own
//! deficit — work-conserving across classes, bounded burst. The 50 ms
//! horizon is derived from the S27 D5 `max ≤ 50 ms` bar: a burst bounded
//! to 50 ms of modeled device time bounds the foreground's queueing
//! delay behind background bytes to one such burst (the class bound the
//! `m2-device-budget` oracle asserts).
//!
//! A model field of 0 means "not probed": that direction is unbudgeted
//! — every admission is `Granted`, every counter still counts — which
//! is the pre-S36 behaviour byte-for-byte and is reported as
//! `io_budget_model:absent`.
//!
//! [`SealPace`] (ADR-0088 D2b) is the foreground *policy* on the same
//! model: the frame-seal rate of a pipelined cell (K > 1) is paced by
//! the device's measured barrier rate so a saturated device sees fewer,
//! larger frames instead of more, smaller ones. It never defers a frame
//! (a cell with no frame in flight always seals); it only lets a due
//! frame keep accumulating for at most one barrier window.

use inf_foundation::time::Nanos;

use crate::token::TokenClass;

/// Every device op a cell issues belongs to exactly one class (ADR-0088
/// D1). Foreground classes are metered, never deferred; background
/// classes are listed in their priority order — the order each protects
/// the foreground (zero-fill guards the barrier class, tier flush
/// guards tail allocation, the checkpoint guards recovery time,
/// compaction's reads guard the disk budget).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum IoClass {
    /// `TokenClass::LogWrite` frames and their linked fsyncs.
    LogFrame = 0,
    /// Synchronous blob-extent writes (no token — charged at the call).
    BlobWrite = 1,
    /// `ReadClass::Foreground` tier reads.
    ColdReadForeground = 2,
    /// `TokenClass::ZeroFillWrite` (ADR-0086 D4).
    ZeroFill = 3,
    /// `TokenClass::TierFlushWrite` rounds and their barriers.
    TierFlush = 4,
    /// `TokenClass::CkptWrite` sections + sidecars, `ManifestSync`.
    Checkpoint = 5,
    /// `ReadClass::Maintain` tier reads (compaction).
    ColdReadMaintain = 6,
}

impl IoClass {
    /// Every class, index order.
    pub const ALL: [IoClass; IoClass::COUNT] = [
        IoClass::LogFrame,
        IoClass::BlobWrite,
        IoClass::ColdReadForeground,
        IoClass::ZeroFill,
        IoClass::TierFlush,
        IoClass::Checkpoint,
        IoClass::ColdReadMaintain,
    ];
    pub const COUNT: usize = 7;

    #[must_use]
    pub const fn index(self) -> usize {
        self as usize
    }

    /// The INFO suffix (`io_budget_bytes_{name}`).
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            IoClass::LogFrame => "log_frame",
            IoClass::BlobWrite => "blob_write",
            IoClass::ColdReadForeground => "cold_read_foreground",
            IoClass::ZeroFill => "zero_fill",
            IoClass::TierFlush => "tier_flush",
            IoClass::Checkpoint => "checkpoint",
            IoClass::ColdReadMaintain => "cold_read_maintain",
        }
    }

    /// Foreground classes are charged and never deferred.
    #[must_use]
    pub const fn is_foreground(self) -> bool {
        matches!(self, IoClass::LogFrame | IoClass::BlobWrite | IoClass::ColdReadForeground)
    }

    /// Reads spend the read direction; everything else the write one.
    #[must_use]
    pub const fn is_read(self) -> bool {
        matches!(self, IoClass::ColdReadForeground | IoClass::ColdReadMaintain)
    }

    /// The class a driver op's token class belongs to — total for every
    /// file class; `None` for socket/wake tokens and for `TierRead`,
    /// whose class is the issuer's `ReadClass` (foreground vs maintain),
    /// not derivable from the token. The simulator's accounting oracle
    /// counts observed bytes by this mapping (ADR-0088 D8).
    #[must_use]
    pub const fn of(token: TokenClass) -> Option<IoClass> {
        match token {
            TokenClass::LogWrite | TokenClass::Fsync => Some(IoClass::LogFrame),
            TokenClass::CkptWrite | TokenClass::CkptSync | TokenClass::ManifestSync => {
                Some(IoClass::Checkpoint)
            }
            TokenClass::TierFlushWrite | TokenClass::TierFlushSync => Some(IoClass::TierFlush),
            TokenClass::ZeroFillWrite => Some(IoClass::ZeroFill),
            TokenClass::TierRead
            | TokenClass::Accept
            | TokenClass::Recv
            | TokenClass::Send
            | TokenClass::Close
            | TokenClass::Wake => None,
        }
    }

    /// Background share weights (ADR-0088 D2): `ZeroFill 4 : TierFlush 4
    /// : Checkpoint 2 : ColdReadMaintain 1`. Foreground weight is 0 —
    /// it is not granted, it is subtracted.
    #[must_use]
    pub const fn weight(self) -> u64 {
        match self {
            IoClass::ZeroFill | IoClass::TierFlush => 4,
            IoClass::Checkpoint => 2,
            IoClass::ColdReadMaintain => 1,
            IoClass::LogFrame | IoClass::BlobWrite | IoClass::ColdReadForeground => 0,
        }
    }
}

/// The device's measured capacity (`io-properties.toml` schema 2,
/// ADR-0088 D6), per device. 0 in a field = not probed ⇒ that direction
/// is unbudgeted.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct DeviceModel {
    pub write_bytes_per_s: u64,
    pub write_ops_per_s: u64,
    pub read_bytes_per_s: u64,
    pub read_ops_per_s: u64,
}

impl DeviceModel {
    /// No probe: every direction unbudgeted (the pre-S36 behaviour).
    pub const ABSENT: DeviceModel = DeviceModel {
        write_bytes_per_s: 0,
        write_ops_per_s: 0,
        read_bytes_per_s: 0,
        read_ops_per_s: 0,
    };

    #[must_use]
    pub const fn is_absent(&self) -> bool {
        self.write_bytes_per_s == 0
            && self.write_ops_per_s == 0
            && self.read_bytes_per_s == 0
            && self.read_ops_per_s == 0
    }

    /// The static per-cell share (L1): each rate divided by the cell
    /// count, computed once at boot. `cells == 0` is treated as 1.
    #[must_use]
    pub const fn share(self, cells: u16) -> DeviceModel {
        let n = if cells == 0 { 1 } else { cells as u64 };
        DeviceModel {
            write_bytes_per_s: self.write_bytes_per_s / n,
            write_ops_per_s: self.write_ops_per_s / n,
            read_bytes_per_s: self.read_bytes_per_s / n,
            read_ops_per_s: self.read_ops_per_s / n,
        }
    }
}

/// The burst horizon (ADR-0088 D2): a class deficit and the shared pool
/// hold at most this much modeled device time. Derived from the S27 D5
/// `max ≤ 50 ms` bar, not tuned.
pub const BURST_HORIZON_NS: u64 = 50_000_000;

/// Under foreground saturation the background grant is clamped at no
/// less than `share / FLOOR_DIVISOR` — the device is shared 7:1.
pub const FLOOR_DIVISOR: u64 = 8;

const NS_PER_S: u128 = 1_000_000_000;

/// The budget's answer to a background class (ADR-0088 D2).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Admission {
    Granted,
    /// Not this slice: the exact shortfall after the class deficit and
    /// the shared pool — the caller re-offers next tick.
    Deferred {
        short_bytes: u64,
        short_ops: u64,
    },
}

/// The smallest slice a class will ever offer — its deficit cap can
/// never be below it, so a deficit can always reach one slice.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ClassSlice {
    pub bytes: u64,
    pub ops: u64,
}

#[derive(Copy, Clone, Debug, Default)]
struct Meter {
    deficit_bytes: u64,
    deficit_ops: u64,
    cap_bytes: u64,
    cap_ops: u64,
    slice_bytes: u64,
    slice_ops: u64,
    spent_bytes: u64,
    spent_ops: u64,
    deferrals: u64,
}

#[derive(Copy, Clone, Debug, Default)]
struct Direction {
    /// The cell's share, per second; 0 = unbudgeted.
    rate_bytes: u64,
    rate_ops: u64,
    pool_bytes: u64,
    pool_ops: u64,
    pool_cap_bytes: u64,
    pool_cap_ops: u64,
    /// Foreground spend since the last refill (subtracted from the grant).
    fg_bytes: u64,
    fg_ops: u64,
    weights: u64,
}

impl Direction {
    const fn budgeted(&self) -> bool {
        self.rate_bytes > 0 || self.rate_ops > 0
    }
}

/// One cell's device budget.
#[derive(Clone, Debug)]
pub struct DeviceBudget {
    write: Direction,
    read: Direction,
    meters: [Meter; IoClass::COUNT],
    last_refill: Nanos,
    model_absent: bool,
}

/// Per-class counters for INFO (ADR-0088 D7).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct ClassCounters {
    pub spent_bytes: u64,
    pub spent_ops: u64,
    pub deferrals: u64,
}

impl DeviceBudget {
    /// `share` is the cell's share (`DeviceModel::share`); `slices`
    /// names each class's smallest offer (foreground entries are
    /// ignored). `now` is the refill origin.
    #[must_use]
    pub fn new(share: DeviceModel, slices: [ClassSlice; IoClass::COUNT], now: Nanos) -> Self {
        let mut write = Direction {
            rate_bytes: share.write_bytes_per_s,
            rate_ops: share.write_ops_per_s,
            ..Direction::default()
        };
        let mut read = Direction {
            rate_bytes: share.read_bytes_per_s,
            rate_ops: share.read_ops_per_s,
            ..Direction::default()
        };
        for class in IoClass::ALL {
            if class.is_foreground() {
                continue;
            }
            let dir = if class.is_read() { &mut read } else { &mut write };
            dir.weights += class.weight();
        }
        write.pool_cap_bytes = horizon_of(write.rate_bytes, 1, 1);
        write.pool_cap_ops = horizon_of(write.rate_ops, 1, 1);
        read.pool_cap_bytes = horizon_of(read.rate_bytes, 1, 1);
        read.pool_cap_ops = horizon_of(read.rate_ops, 1, 1);
        let mut meters = [Meter::default(); IoClass::COUNT];
        for class in IoClass::ALL {
            let m = &mut meters[class.index()];
            let slice = slices[class.index()];
            m.slice_bytes = slice.bytes;
            m.slice_ops = slice.ops;
            if class.is_foreground() {
                continue;
            }
            let dir = if class.is_read() { &read } else { &write };
            m.cap_bytes = horizon_of(dir.rate_bytes, class.weight(), dir.weights).max(slice.bytes);
            m.cap_ops = horizon_of(dir.rate_ops, class.weight(), dir.weights).max(slice.ops.max(1));
            // Start full: the first slice of every class is granted at
            // boot (the cap is one burst horizon, by construction).
            m.deficit_bytes = m.cap_bytes;
            m.deficit_ops = m.cap_ops;
        }
        DeviceBudget { write, read, meters, last_refill: now, model_absent: share.is_absent() }
    }

    /// True when no direction is budgeted (INFO `io_budget_model:absent`).
    #[must_use]
    pub const fn model_absent(&self) -> bool {
        self.model_absent
    }

    /// The cell's write/read byte shares per second (INFO).
    #[must_use]
    pub const fn share_bytes_per_s(&self) -> (u64, u64) {
        (self.write.rate_bytes, self.read.rate_bytes)
    }

    /// Once per MAINTAIN entry (ADR-0088 D2). Time moving backwards or
    /// not at all is a no-op (iteration-quantized clock).
    pub fn refill(&mut self, now: Nanos) {
        let elapsed = now.0.saturating_sub(self.last_refill.0);
        if elapsed == 0 {
            return;
        }
        self.last_refill = now;
        for is_read in [false, true] {
            let dir = if is_read { &mut self.read } else { &mut self.write };
            if !dir.budgeted() {
                dir.fg_bytes = 0;
                dir.fg_ops = 0;
                continue;
            }
            let grant_bytes = grant(dir.rate_bytes, elapsed, dir.fg_bytes);
            let grant_ops = grant(dir.rate_ops, elapsed, dir.fg_ops);
            dir.fg_bytes = 0;
            dir.fg_ops = 0;
            let mut overflow_bytes: u64 = 0;
            let mut overflow_ops: u64 = 0;
            for class in IoClass::ALL {
                if class.is_foreground() || class.is_read() != is_read {
                    continue;
                }
                let m = &mut self.meters[class.index()];
                let add_bytes = mul_div(grant_bytes, class.weight(), dir.weights);
                let add_ops = mul_div(grant_ops, class.weight(), dir.weights);
                let room_bytes = m.cap_bytes.saturating_sub(m.deficit_bytes);
                let room_ops = m.cap_ops.saturating_sub(m.deficit_ops);
                m.deficit_bytes += add_bytes.min(room_bytes);
                m.deficit_ops += add_ops.min(room_ops);
                overflow_bytes =
                    overflow_bytes.saturating_add(add_bytes.saturating_sub(room_bytes));
                overflow_ops = overflow_ops.saturating_add(add_ops.saturating_sub(room_ops));
            }
            dir.pool_bytes = dir.pool_bytes.saturating_add(overflow_bytes).min(dir.pool_cap_bytes);
            dir.pool_ops = dir.pool_ops.saturating_add(overflow_ops).min(dir.pool_cap_ops);
        }
    }

    /// Offer `bytes`/`ops` for `class`. Foreground: charged, `Granted`.
    /// Background: `Granted` iff the class deficit plus the shared pool
    /// covers both dimensions (then they are deducted and counted as
    /// spent), else `Deferred` with the exact shortfall (counted).
    pub fn admit(&mut self, class: IoClass, bytes: u64, ops: u64) -> Admission {
        let is_read = class.is_read();
        let dir = if is_read { &mut self.read } else { &mut self.write };
        let m = &mut self.meters[class.index()];
        if class.is_foreground() {
            dir.fg_bytes = dir.fg_bytes.saturating_add(bytes);
            dir.fg_ops = dir.fg_ops.saturating_add(ops);
            m.spent_bytes = m.spent_bytes.saturating_add(bytes);
            m.spent_ops = m.spent_ops.saturating_add(ops);
            return Admission::Granted;
        }
        if !dir.budgeted() {
            m.spent_bytes = m.spent_bytes.saturating_add(bytes);
            m.spent_ops = m.spent_ops.saturating_add(ops);
            return Admission::Granted;
        }
        // An unbudgeted dimension (rate 0) never defers on that axis.
        let need_bytes =
            if dir.rate_bytes == 0 { 0 } else { bytes.saturating_sub(m.deficit_bytes) };
        let need_ops = if dir.rate_ops == 0 { 0 } else { ops.saturating_sub(m.deficit_ops) };
        let short_bytes = need_bytes.saturating_sub(dir.pool_bytes);
        let short_ops = need_ops.saturating_sub(dir.pool_ops);
        if short_bytes > 0 || short_ops > 0 {
            m.deferrals += 1;
            return Admission::Deferred { short_bytes, short_ops };
        }
        if dir.rate_bytes > 0 {
            m.deficit_bytes = m.deficit_bytes.saturating_sub(bytes);
            dir.pool_bytes -= need_bytes;
        }
        if dir.rate_ops > 0 {
            m.deficit_ops = m.deficit_ops.saturating_sub(ops);
            dir.pool_ops -= need_ops;
        }
        m.spent_bytes = m.spent_bytes.saturating_add(bytes);
        m.spent_ops = m.spent_ops.saturating_add(ops);
        Admission::Granted
    }

    /// Spend unconditionally: the op is issued whatever the deficit says
    /// (a checkpoint header — its file is already created — or any
    /// foreground op). A background class's deficit absorbs it (to zero,
    /// never below), so its next offer pays for the overrun; the counters
    /// stay exact. Never a deferral.
    pub fn charge(&mut self, class: IoClass, bytes: u64, ops: u64) {
        let is_read = class.is_read();
        let dir = if is_read { &mut self.read } else { &mut self.write };
        let m = &mut self.meters[class.index()];
        m.spent_bytes = m.spent_bytes.saturating_add(bytes);
        m.spent_ops = m.spent_ops.saturating_add(ops);
        if class.is_foreground() {
            dir.fg_bytes = dir.fg_bytes.saturating_add(bytes);
            dir.fg_ops = dir.fg_ops.saturating_add(ops);
            return;
        }
        if dir.rate_bytes > 0 {
            m.deficit_bytes = m.deficit_bytes.saturating_sub(bytes);
        }
        if dir.rate_ops > 0 {
            m.deficit_ops = m.deficit_ops.saturating_sub(ops);
        }
    }

    /// Return the part of a granted offer that was not issued (a tier
    /// round staged fewer bytes than its slice bound): tokens go back to
    /// the class deficit (capped), the spent counters are corrected.
    /// Foreground classes correct their counters only.
    pub fn refund(&mut self, class: IoClass, bytes: u64, ops: u64) {
        let is_read = class.is_read();
        let dir = if is_read { &mut self.read } else { &mut self.write };
        let m = &mut self.meters[class.index()];
        m.spent_bytes = m.spent_bytes.saturating_sub(bytes);
        m.spent_ops = m.spent_ops.saturating_sub(ops);
        if class.is_foreground() {
            dir.fg_bytes = dir.fg_bytes.saturating_sub(bytes);
            dir.fg_ops = dir.fg_ops.saturating_sub(ops);
            return;
        }
        if dir.rate_bytes > 0 {
            m.deficit_bytes = m.deficit_bytes.saturating_add(bytes).min(m.cap_bytes);
        }
        if dir.rate_ops > 0 {
            m.deficit_ops = m.deficit_ops.saturating_add(ops).min(m.cap_ops);
        }
    }

    /// Per-class counters (INFO).
    #[must_use]
    pub fn counters(&self, class: IoClass) -> ClassCounters {
        let m = &self.meters[class.index()];
        ClassCounters { spent_bytes: m.spent_bytes, spent_ops: m.spent_ops, deferrals: m.deferrals }
    }

    /// The class's current deficit (tests / INFO).
    #[must_use]
    pub fn deficit(&self, class: IoClass) -> (u64, u64) {
        let m = &self.meters[class.index()];
        (m.deficit_bytes, m.deficit_ops)
    }

    /// The direction's shared pool (tests).
    #[must_use]
    pub fn pool(&self, read: bool) -> (u64, u64) {
        let dir = if read { &self.read } else { &self.write };
        (dir.pool_bytes, dir.pool_ops)
    }
}

/// `rate × elapsed_ns / 1e9 − foreground`, clamped at ≥ the floor share.
fn grant(rate: u64, elapsed_ns: u64, foreground: u64) -> u64 {
    if rate == 0 {
        return 0;
    }
    let full =
        u64::try_from(u128::from(rate) * u128::from(elapsed_ns) / NS_PER_S).unwrap_or(u64::MAX);
    let floor = full / FLOOR_DIVISOR;
    full.saturating_sub(foreground).max(floor)
}

/// `rate × weight / weights × BURST_HORIZON`.
fn horizon_of(rate: u64, weight: u64, weights: u64) -> u64 {
    if rate == 0 || weights == 0 {
        return 0;
    }
    let per_s = mul_div(rate, weight, weights);
    u64::try_from(u128::from(per_s) * u128::from(BURST_HORIZON_NS) / NS_PER_S).unwrap_or(u64::MAX)
}

fn mul_div(value: u64, num: u64, den: u64) -> u64 {
    if den == 0 {
        return 0;
    }
    u64::try_from(u128::from(value) * u128::from(num) / u128::from(den)).unwrap_or(u64::MAX)
}

/// The frame-seal pacer (ADR-0088 D2b): a token bucket refilled at the
/// cell's share of the device's concurrent barrier rate, capacity K.
/// `take` is the LOG step's question "may a second frame seal now?" —
/// a cell with nothing in flight never asks.
#[derive(Clone, Debug)]
pub struct SealPace {
    /// Nanoseconds per token (0 = disabled: every `take` succeeds).
    ns_per_token: u64,
    capacity: u32,
    tokens: u32,
    /// Accrued nanoseconds toward the next token.
    credit_ns: u64,
    last: Nanos,
    waits: u64,
}

impl SealPace {
    /// `barriers_per_s` is the cell's share of `write_ops_per_s_4k_qd4`
    /// (0 = disabled); `capacity` is the pipeline depth K.
    #[must_use]
    pub fn new(barriers_per_s: u64, capacity: u32, now: Nanos) -> SealPace {
        let ns_per_token = if barriers_per_s == 0 {
            0
        } else {
            u64::try_from(NS_PER_S / u128::from(barriers_per_s)).unwrap_or(u64::MAX).max(1)
        };
        let capacity = capacity.max(1);
        SealPace { ns_per_token, capacity, tokens: capacity, credit_ns: 0, last: now, waits: 0 }
    }

    #[must_use]
    pub const fn enabled(&self) -> bool {
        self.ns_per_token > 0
    }

    fn refill(&mut self, now: Nanos) {
        if self.ns_per_token == 0 || self.tokens >= self.capacity {
            self.last = now;
            self.credit_ns = 0;
            return;
        }
        let elapsed = now.0.saturating_sub(self.last.0);
        self.last = now;
        self.credit_ns = self.credit_ns.saturating_add(elapsed);
        let earned = self.credit_ns / self.ns_per_token;
        if earned > 0 {
            let earned = u32::try_from(earned).unwrap_or(u32::MAX);
            self.tokens = self.tokens.saturating_add(earned).min(self.capacity);
            self.credit_ns %= self.ns_per_token;
            if self.tokens >= self.capacity {
                self.credit_ns = 0;
            }
        }
    }

    /// Take a token at `now`. Disabled ⇒ always true. A refusal is one
    /// wait episode when `held` was false (the caller's hold flag).
    pub fn take(&mut self, now: Nanos, held: bool) -> bool {
        if self.ns_per_token == 0 {
            return true;
        }
        self.refill(now);
        if self.tokens > 0 {
            self.tokens -= 1;
            true
        } else {
            self.waits += u64::from(!held);
            false
        }
    }

    /// Wait episodes (INFO `frame_waits_pace`).
    #[must_use]
    pub const fn waits(&self) -> u64 {
        self.waits
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1 << 20;

    fn slices() -> [ClassSlice; IoClass::COUNT] {
        let mut s = [ClassSlice { bytes: 0, ops: 0 }; IoClass::COUNT];
        s[IoClass::ZeroFill.index()] = ClassSlice { bytes: 256 << 10, ops: 1 };
        s[IoClass::TierFlush.index()] = ClassSlice { bytes: MIB, ops: 256 };
        s[IoClass::Checkpoint.index()] = ClassSlice { bytes: 256 << 10, ops: 1 };
        s[IoClass::ColdReadMaintain.index()] = ClassSlice { bytes: 16 << 10, ops: 1 };
        s
    }

    fn model() -> DeviceModel {
        // 400 MiB/s, 8k ops/s per device; read 200 MiB/s, 20k ops/s.
        DeviceModel {
            write_bytes_per_s: 400 * MIB,
            write_ops_per_s: 8_000,
            read_bytes_per_s: 200 * MIB,
            read_ops_per_s: 20_000,
        }
    }

    fn budget() -> DeviceBudget {
        DeviceBudget::new(model().share(4), slices(), Nanos(0))
    }

    #[test]
    fn share_divides_every_rate_by_the_cell_count() {
        let s = model().share(4);
        assert_eq!(s.write_bytes_per_s, 100 * MIB);
        assert_eq!(s.write_ops_per_s, 2_000);
        assert_eq!(s.read_bytes_per_s, 50 * MIB);
        assert_eq!(model().share(0), model().share(1));
        assert!(DeviceModel::ABSENT.is_absent());
        assert!(!model().is_absent());
    }

    #[test]
    fn an_absent_model_grants_everything_and_still_counts() {
        let mut b = DeviceBudget::new(DeviceModel::ABSENT, slices(), Nanos(0));
        assert!(b.model_absent());
        for _ in 0..1_000 {
            assert_eq!(b.admit(IoClass::Checkpoint, 64 * MIB, 1), Admission::Granted);
        }
        let c = b.counters(IoClass::Checkpoint);
        assert_eq!(c.spent_bytes, 64_000 * MIB);
        assert_eq!(c.spent_ops, 1_000);
        assert_eq!(c.deferrals, 0);
    }

    #[test]
    fn caps_are_one_burst_horizon_and_never_below_one_slice() {
        let b = budget();
        // Checkpoint: 100 MiB/s × 2/10 × 50 ms ≈ 1 MiB > its 256 KiB slice.
        let (cap_bytes, cap_ops) = b.deficit(IoClass::Checkpoint);
        assert_eq!(cap_bytes, horizon_of(100 * MIB, 2, 10));
        assert!(cap_bytes > 256 << 10);
        // ops: 2000 × 2/10 × 50 ms = 20 ≥ the 1-op slice.
        assert_eq!(cap_ops, 20);
        // Tier flush's slice (1 MiB) exceeds its horizon share? 100 MiB/s ×
        // 4/10 × 50 ms ≈ 2 MiB — horizon wins; ops: 2000×4/10×0.05 = 40 <
        // its 256-op slice — the slice wins.
        let (tf_bytes, tf_ops) = b.deficit(IoClass::TierFlush);
        assert_eq!(tf_bytes, horizon_of(100 * MIB, 4, 10));
        assert_eq!(tf_ops, 256);
    }

    #[test]
    fn a_class_past_its_cap_overflows_into_the_shared_pool() {
        let mut b = budget();
        // Everything starts full; a second of idle refill goes entirely
        // to the pool, which caps at 50 ms of the share.
        b.refill(Nanos(1_000_000_000));
        let (pool_bytes, pool_ops) = b.pool(false);
        assert_eq!(pool_bytes, horizon_of(100 * MIB, 1, 1));
        assert_eq!(pool_ops, 100);
        // A busy class drains its deficit, then the pool (work-conserving).
        let (own, _) = b.deficit(IoClass::Checkpoint);
        assert_eq!(b.admit(IoClass::Checkpoint, own + pool_bytes, 2), Admission::Granted);
        assert_eq!(b.deficit(IoClass::Checkpoint).0, 0);
        assert_eq!(b.pool(false).0, 0);
        // Nothing left: the exact shortfall is reported and counted.
        assert_eq!(
            b.admit(IoClass::Checkpoint, 4096, 1),
            Admission::Deferred { short_bytes: 4096, short_ops: 0 }
        );
        assert_eq!(b.counters(IoClass::Checkpoint).deferrals, 1);
    }

    #[test]
    fn foreground_spend_reduces_the_grant_to_the_floor_and_no_further() {
        let mut b = budget();
        // Drain the checkpoint class and the pool first.
        b.admit(IoClass::Checkpoint, b.deficit(IoClass::Checkpoint).0, 0);
        assert_eq!(b.pool(false).0, 0);
        // Foreground eats 10× the share over 10 ms; the grant clamps at
        // the floor (1/8 of 10 ms of share = 128 KiB), split by weight.
        assert_eq!(b.admit(IoClass::LogFrame, 10 * MIB, 100), Admission::Granted);
        b.refill(Nanos(10_000_000));
        let ten_ms_share = 100 * MIB / 100;
        let floor = ten_ms_share / FLOOR_DIVISOR;
        let expected = mul_div(floor, 2, 10);
        assert_eq!(b.deficit(IoClass::Checkpoint).0, expected);
        // With no foreground spend the full 10 ms share is granted.
        b.admit(IoClass::Checkpoint, expected, 0);
        b.refill(Nanos(20_000_000));
        assert_eq!(b.deficit(IoClass::Checkpoint).0, mul_div(ten_ms_share, 2, 10));
        // The foreground is counted, never deferred.
        let fg = b.counters(IoClass::LogFrame);
        assert_eq!((fg.spent_bytes, fg.spent_ops, fg.deferrals), (10 * MIB, 100, 0));
    }

    #[test]
    fn a_refund_returns_tokens_up_to_the_cap_and_corrects_the_counters() {
        let mut b = budget();
        let (cap, _) = b.deficit(IoClass::TierFlush);
        assert_eq!(b.admit(IoClass::TierFlush, MIB, 256), Admission::Granted);
        assert_eq!(b.deficit(IoClass::TierFlush).0, cap - MIB);
        // Staged only 300 KiB of the 1 MiB slice bound, 40 of 256 ops.
        b.refund(IoClass::TierFlush, MIB - (300 << 10), 216);
        assert_eq!(b.deficit(IoClass::TierFlush).0, cap - (300 << 10));
        let c = b.counters(IoClass::TierFlush);
        assert_eq!((c.spent_bytes, c.spent_ops), (300 << 10, 40));
        // A refund never lifts the deficit past its cap.
        b.refund(IoClass::TierFlush, 100 * MIB, 0);
        assert_eq!(b.deficit(IoClass::TierFlush).0, cap);
    }

    #[test]
    fn an_unconditional_charge_drains_the_deficit_and_counts() {
        let mut b = budget();
        let (cap, _) = b.deficit(IoClass::Checkpoint);
        b.charge(IoClass::Checkpoint, cap + 4096, 1);
        assert_eq!(b.deficit(IoClass::Checkpoint).0, 0, "absorbed to zero, never below");
        assert_eq!(b.counters(IoClass::Checkpoint).spent_bytes, cap + 4096);
        // The next offer pays for the overrun.
        assert!(matches!(b.admit(IoClass::Checkpoint, 4096, 1), Admission::Deferred { .. }));
    }

    #[test]
    fn reads_and_writes_are_separate_directions() {
        let mut b = budget();
        let (rb, _) = b.deficit(IoClass::ColdReadMaintain);
        assert_eq!(rb, horizon_of(50 * MIB, 1, 1).max(16 << 10));
        // A write-side drain leaves the read side untouched.
        b.admit(IoClass::ZeroFill, b.deficit(IoClass::ZeroFill).0, 1);
        assert_eq!(b.deficit(IoClass::ColdReadMaintain).0, rb);
    }

    #[test]
    fn an_unbudgeted_dimension_never_defers_on_that_axis() {
        let m = DeviceModel { write_bytes_per_s: 100 * MIB, ..DeviceModel::ABSENT };
        let mut b = DeviceBudget::new(m, slices(), Nanos(0));
        // ops rate is 0: a million ops is fine, bytes still bind.
        assert_eq!(b.admit(IoClass::Checkpoint, 4096, 1_000_000), Admission::Granted);
        let left = b.deficit(IoClass::Checkpoint).0;
        assert!(matches!(
            b.admit(IoClass::Checkpoint, left + 1, 0),
            Admission::Deferred { short_bytes: 1, short_ops: 0 }
        ));
    }

    #[test]
    fn refill_is_monotone_and_a_stalled_clock_is_a_no_op() {
        let mut b = budget();
        b.admit(IoClass::ZeroFill, b.deficit(IoClass::ZeroFill).0, 1);
        b.refill(Nanos(0));
        assert_eq!(b.deficit(IoClass::ZeroFill).0, 0);
        b.refill(Nanos(5_000_000));
        let after = b.deficit(IoClass::ZeroFill).0;
        assert_eq!(after, mul_div(100 * MIB / 200, 4, 10));
        // Backwards time: nothing moves.
        b.refill(Nanos(1_000_000));
        assert_eq!(b.deficit(IoClass::ZeroFill).0, after);
    }

    #[test]
    fn two_budgets_fed_the_same_sequence_agree_exactly() {
        let mut a = budget();
        let mut b = budget();
        let mut now = 0u64;
        let mut seed = 0x9E37_79B9_7F4A_7C15u64;
        for _ in 0..10_000 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            now += seed % 3_000_000;
            let class = IoClass::ALL[(seed >> 8) as usize % IoClass::COUNT];
            let bytes = (seed >> 16) % (2 * MIB);
            let ops = (seed >> 40) % 8;
            a.refill(Nanos(now));
            b.refill(Nanos(now));
            assert_eq!(a.admit(class, bytes, ops), b.admit(class, bytes, ops));
        }
        for class in IoClass::ALL {
            assert_eq!(a.counters(class), b.counters(class));
        }
    }

    #[test]
    fn seal_pace_paces_a_pipelined_cell_and_never_a_drained_one() {
        // 2000 barriers/s ⇒ one token per 500 µs, capacity 4.
        let mut p = SealPace::new(2_000, 4, Nanos(0));
        assert!(p.enabled());
        for _ in 0..4 {
            assert!(p.take(Nanos(0), false));
        }
        assert!(!p.take(Nanos(0), false));
        assert!(!p.take(Nanos(100_000), true));
        assert_eq!(p.waits(), 1, "episodes, not LOG steps");
        assert!(p.take(Nanos(500_000), false));
        assert!(!p.take(Nanos(500_000), false));
        // A long idle refills to capacity, never beyond.
        for _ in 0..4 {
            assert!(p.take(Nanos(10_000_000), false));
        }
        assert!(!p.take(Nanos(10_000_000), false));
        // Disabled: every take succeeds.
        let mut off = SealPace::new(0, 4, Nanos(0));
        assert!(!off.enabled());
        for _ in 0..100 {
            assert!(off.take(Nanos(0), false));
        }
        assert_eq!(off.waits(), 0);
    }
}
