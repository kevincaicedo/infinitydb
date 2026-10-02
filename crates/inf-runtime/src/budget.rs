//! Per-cell device budget (M4.5-S36, ADR-0088 D1; the grant rule is
//! ADR-0178 D2): device I/O classes over a measured device model, spent
//! through per-class byte and op credit refilled on the injected clock.
//!
//! The shape is Seastar's io-queue with the one change L1 demands:
//! shares are **static per cell** (the device model is divided by the
//! cell count at boot; nothing here is shared between cells), so the
//! structure is a plain owned value the plane refills once per MAINTAIN
//! entry from `LoopCx::now` and consults at the four background issuing
//! sites. Foreground classes are *metered* (charged, so the background
//! grant is work-conserving toward the log) and **never deferred**. A
//! background producer calls [`DeviceBudget::offer`] and gets an
//! [`Issue`]: `Now`, or `NotThisSlice` — keep the offer and re-offer it
//! next slice with nothing moved. Nothing queues, nothing allocates,
//! nothing waits.
//!
//! **The grant rule (ADR-0178 D2).** Per direction (write, read) and per
//! axis (bytes, ops), all integers:
//!
//! - *Refill*: the grant for the elapsed interval is the cell's share of
//!   the modeled rate minus what the foreground spent since the last
//!   refill, never below one [`FLOOR_DIVISOR`]th of the share (a
//!   background that cannot run at all is a recovery-time bug, a
//!   foreground stall, or a class downgrade). The floor is a lower bound
//!   on the weighted grant, not a ratio the device is held to: the
//!   checkpoint's keep-up grant below is paid on top of it. Carries keep
//!   every sub-unit remainder, so any refill interval grants its exact
//!   share over time.
//! - *Split*: by weight among the background classes; each class's
//!   credit is capped at `cap = max(slice, share × weight/Σw × horizon)`,
//!   and what a capped class cannot hold flows to a per-direction pool
//!   (capped at `share × horizon`) any class draws after its own credit.
//!   The 50 ms horizon is derived from the S27 D5 `max ≤ 50 ms` bar: a
//!   burst bounded to 50 ms of modeled device time bounds the
//!   foreground's queueing behind background bytes to one such burst.
//! - *Attainable*: an offer at most `cap` on every budgeted axis is
//!   granted from the class's credit, then the pool, or answered
//!   `NotThisSlice`. A class that owes is granted nothing.
//! - *Overrun*: an offer above `cap` can never be covered by the class's
//!   own credit, and the pool is not its to wait on — another class may
//!   always drain it first. It is issued by a counted overrun once the
//!   class is `Rested` at its cap: its credit, then what the pool holds,
//!   and the rest is **owed** — repaid by refills before the credit grows
//!   again. `Rest` is written by refill alone: `Rested` when every
//!   budgeted axis already held its cap before the refill's grant. A draw
//!   ends the rest pass (it leaves the class below its cap); a grant
//!   refunded in full does not.
//! - *Charge*: spend is unconditional — a checkpoint header or barrier,
//!   or a tier round's bytes staged past its grant — and what the credit
//!   cannot hold is owed.
//! - *Receipt*: the budget remembers the last background `Now` answer's
//!   draws; the refund directly after it returns at most that, the debt
//!   first, then the pool, then the credit — so a full refund is the
//!   grant's exact inverse. Every other call voids the receipt.
//!
//! **The checkpoint keep-up floor** (ADR-0178 D2): under foreground
//! saturation a weighted share alone starved the checkpoint class (340 k
//! deferrals, no publish in 20 s of 270 k ops/s) — a retained log that
//! grows for as long as saturation lasts, i.e. an unbounded recovery
//! tail. The checkpoint's grant is
//! therefore floored at `foreground_write_bytes_since_refill / α`, the
//! bytes a checkpoint must write to stay inside the interval it is
//! triggered at (`interval = α × ckpt_bytes_last`, ADR-0088 D4): the
//! device's write capacity splits `α : 1` between the log and the
//! checkpoint at saturation — the 1.5× write-amplification model made
//! arithmetic — and the checkpoint always completes within α intervals.
//! The keep-up grant is refilled like any other: what a checkpoint
//! resting at its cap cannot hold overflows to the write pool, where the
//! tier and zero-fill classes may draw it. With the checkpoint idle and
//! the log at the whole share, those classes can therefore issue well
//! above their own floor share (ADR-0178 D2, as designed).
//! The floor and the weighted share are compared exactly and rounded
//! once, with one remainder carried (ADR-0178 D2): rounding the floor on
//! its own lost up to `(α − 1)/α` byte a refill where the two cross.
//! Zero-fill has no such floor: its shortfall is a visible class
//! downgrade (`rotations_unzeroed`), never a correctness term.
//!
//! **Progress (ADR-0178 D4).** A class with one producer on the cell
//! has every offer issued within `(owed + cap + charges + 3) / r + 2Δ` of
//! injected time, `r` the class's floored weighted share (on the
//! checkpoint's bytes, the larger of that and the keep-up crossover), 3
//! units the carries' lag, and `Δ` the longest refill interval: one for
//! quantization, one for the rest pass.
//!
//! A model field of 0 means "not probed": that direction is unbudgeted
//! — every offer is `Now`, every counter still counts — which is the
//! pre-S36 behaviour byte-for-byte and is reported as
//! `io_budget_model:absent`.
//!
//! [`SealPace`] (ADR-0088 D2b) is the foreground *policy* on the same
//! model: the frame-seal rate of a pipelined cell (K > 1) is paced by
//! the device's measured barrier rate so a saturated device sees fewer,
//! larger frames instead of more, smaller ones. It never defers a frame
//! (a cell with no frame in flight always seals); it only lets a due
//! frame keep accumulating for at most one barrier window.

use std::num::NonZeroU64;

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

    /// Background share weights (ADR-0178 D2): `ZeroFill 4 : TierFlush 4
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

/// The burst horizon (ADR-0178 D2): a class's credit and the shared pool
/// hold at most this much modeled device time. Derived from the S27 D5
/// `max ≤ 50 ms` bar, not tuned.
pub const BURST_HORIZON_NS: u64 = 50_000_000;

/// Under foreground saturation the weighted background grant is clamped
/// at no less than `share / FLOOR_DIVISOR`. A lower bound only: the
/// checkpoint's keep-up grant and its overflow into the pool are paid
/// above it.
pub const FLOOR_DIVISOR: u64 = 8;

const NS_PER_S: u128 = 1_000_000_000;

/// The two axes of a direction, as indices into its per-axis arrays.
const BYTES: usize = 0;
const OPS: usize = 1;
const AXES: [usize; 2] = [BYTES, OPS];

/// A background producer's answer (ADR-0178 D1): issue now, or keep the
/// offer and re-offer it next slice with nothing moved. It has two
/// variants because a producer has two answers; a bounded wait and an
/// overrun's wait differ only in the counters. Not comparable outside
/// this crate's tests: a producer matches it, and must — an answer
/// dropped unread would issue on `NotThisSlice` without credit.
#[derive(Copy, Clone, Debug)]
#[cfg_attr(test, derive(PartialEq, Eq))]
#[must_use = "a producer issues only on `Issue::Now` (ADR-0178 D1)"]
pub enum Issue {
    Now,
    NotThisSlice,
}

/// A background class's cap (ADR-0178 D2): the most its own credit can
/// hold on each axis. An offer above it on a budgeted axis is issued
/// only by an overrun.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ClassCap {
    pub bytes: u64,
    pub ops: u64,
}

/// The smallest slice a class will ever offer — its cap can never be
/// below it, so a class's credit can always reach one slice.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ClassSlice {
    pub bytes: u64,
    pub ops: u64,
}

/// The three outcomes of an offer, resolved by [`DeviceBudget::offer`]
/// alone (ADR-0178 D1). Private: no producer can compare or collapse
/// them.
#[derive(Copy, Clone, Debug)]
#[cfg_attr(test, derive(PartialEq, Eq))]
enum Admission {
    Granted,
    /// A wait D4 bounds: the credit plus the pool fall short.
    Deferred {
        short_bytes: u64,
        short_ops: u64,
    },
    /// Above the class cap on a budgeted axis: waiting cannot end it.
    Unattainable {
        cap_bytes: u64,
        cap_ops: u64,
    },
}

/// The overrun's two outcomes (ADR-0178 D2).
#[derive(Copy, Clone, Debug)]
#[cfg_attr(test, derive(PartialEq, Eq))]
enum Overrun {
    /// Issued now; the part the credit and the pool could not cover is owed.
    Granted { owed_bytes: u64, owed_ops: u64 },
    /// Not `Rested` at its cap: the credit the class lacks to reach it
    /// (zero on both axes while only the rest pass is missing).
    Deferred { short_bytes: u64, short_ops: u64 },
}

/// One axis of a background class's credit (ADR-0178 D2). `Held(h)`
/// holds `h ≤ cap`; a debt is never zero. The type makes "holds and
/// owes" unrepresentable.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Credit {
    Held(u64),
    Owed(NonZeroU64),
}

impl Credit {
    /// `amount` owed, or `Held(0)` when nothing is.
    fn owing(amount: u64) -> Credit {
        NonZeroU64::new(amount).map_or(Credit::Held(0), Credit::Owed)
    }

    /// What the class holds (0 while it owes).
    const fn held(self) -> u64 {
        match self {
            Credit::Held(held) => held,
            Credit::Owed(_) => 0,
        }
    }

    /// What the class owes (0 while it holds).
    const fn owed(self) -> u64 {
        match self {
            Credit::Held(_) => 0,
            Credit::Owed(owed) => owed.get(),
        }
    }

    /// The refill row: a debt is repaid first; the credit then grows to
    /// `cap`. Returns the new credit and what `cap` could not hold.
    fn refilled(self, grant: u64, cap: u64) -> (Credit, u64) {
        let free = match self {
            Credit::Held(held) => held.saturating_add(grant),
            Credit::Owed(owed) if grant < owed.get() => {
                return (Credit::owing(owed.get() - grant), 0);
            }
            Credit::Owed(owed) => grant - owed.get(),
        };
        (Credit::Held(free.min(cap)), free.saturating_sub(cap))
    }

    /// The charge row: spend beyond what is held is owed.
    fn charged(self, amount: u64) -> Credit {
        match self {
            Credit::Held(held) if amount <= held => Credit::Held(held - amount),
            Credit::Held(held) => Credit::owing(amount - held),
            Credit::Owed(owed) => Credit::owing(owed.get().saturating_add(amount)),
        }
    }

    /// The refund row's credit half: `debt` settles what the grant owed,
    /// then `credit` returns to what it held, never above `cap`.
    fn refunded(self, debt: u64, credit: u64, cap: u64) -> Credit {
        match self {
            Credit::Owed(owed) if debt < owed.get() => Credit::owing(owed.get() - debt),
            Credit::Owed(_) => Credit::Held(credit.min(cap)),
            Credit::Held(held) => Credit::Held(held.saturating_add(credit).min(cap)),
        }
    }
}

/// A background class's rest pass (ADR-0178 D2), written by refill
/// alone: `Rested` when every budgeted axis already held its cap before
/// the refill added its grant. An overrun needs it.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Rest {
    Rested,
    Filling,
}

/// What one grant took on one axis: from the class's credit, from the
/// pool, and as debt.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
struct Draw {
    credit: u64,
    pool: u64,
    debt: u64,
}

/// The last background `Now` answer (ADR-0178 D2): its class, what it
/// granted per axis, and per budgeted axis what it drew. A refund
/// directly after its grant returns at most this, last-taken first; any
/// other call voids it.
#[derive(Copy, Clone, Debug)]
struct Receipt {
    class: IoClass,
    granted: [u64; 2],
    draws: [Draw; 2],
}

#[derive(Copy, Clone, Debug)]
struct Meter {
    /// Per axis; `Held(cap)` at boot.
    credit: [Credit; 2],
    cap: [u64; 2],
    slice: [u64; 2],
    rest: Rest,
    /// Issued bytes and ops over the cell's life. Width policy: saturates
    /// at `u64::MAX` (16 EiB, centuries at any device rate); past it a
    /// refund's correction is no longer exact.
    spent: [u64; 2],
    /// `NotThisSlice` answers over the cell's life. Width policy: one
    /// increment per call, so 2⁶⁴ calls — 5 800 years at one a
    /// nanosecond — is unreachable and the add is plain.
    deferrals: u64,
    /// Offers above the cap over the cell's life; the same width policy
    /// as `deferrals`.
    unattainable: u64,
    /// `B − cap` summed over the cell's overruns. Width policy: saturates
    /// at `u64::MAX` — one call can add up to `u64::MAX − cap` — and is
    /// an approximate total past it; the live debt is the class's credit.
    overrun_bytes: u64,
    /// Sub-unit remainders of the weighted share (mod `weights`), carried
    /// across refills so a grant too small to divide still accrues.
    carry: [u64; 2],
}

impl Default for Meter {
    fn default() -> Meter {
        Meter {
            credit: [Credit::Held(0); 2],
            cap: [0; 2],
            slice: [0; 2],
            rest: Rest::Rested,
            spent: [0; 2],
            deferrals: 0,
            unattainable: 0,
            overrun_bytes: 0,
            carry: [0; 2],
        }
    }
}

/// The sub-unit remainders one axis of a direction carries across
/// refills (M4.5-S39d): `ns` is the unspent `rate × elapsed` product
/// below one unit (mod 10⁹), `floor` the unspent eighths of the
/// saturation floor (mod `FLOOR_DIVISOR`). A loop iterating faster than
/// one unit per refill granted zero per refill and zero forever — the
/// idle-node checkpoint starvation the S39d boundary checkpoint found.
#[derive(Copy, Clone, Debug, Default)]
struct Carry {
    ns: u64,
    floor: u64,
}

#[derive(Copy, Clone, Debug, Default)]
struct Direction {
    /// The cell's share per second, per axis; 0 = that axis unbudgeted.
    rate: [u64; 2],
    pool: [u64; 2],
    pool_cap: [u64; 2],
    /// Foreground spend since the last refill (subtracted from the grant).
    fg: [u64; 2],
    weights: u64,
    carry: [Carry; 2],
    /// Refills whose grant the saturation floor set (the class oracle's
    /// engagement witness for its foreground regime).
    #[cfg(test)]
    floor_bound: u64,
    /// Refills whose checkpoint byte grant the keep-up term set (the
    /// class oracle's engagement witness for its crossover regime).
    #[cfg(test)]
    keepup_bound: u64,
}

impl Direction {
    const fn budgeted(&self) -> bool {
        self.rate[BYTES] > 0 || self.rate[OPS] > 0
    }

    /// The axis grant for `elapsed_ns`: `rate × elapsed` less the
    /// foreground spend, never below the saturation floor (one eighth of
    /// the full grant). Both the product and the floor carry their
    /// remainders so every refill interval, however short, grants its
    /// exact share over time (M4.5-S39d).
    fn axis_grant(&mut self, axis: usize, elapsed_ns: u64, foreground: u64) -> u64 {
        let rate = self.rate[axis];
        let carry = &mut self.carry[axis];
        if rate == 0 {
            *carry = Carry::default();
            return 0;
        }
        let scaled = u128::from(rate) * u128::from(elapsed_ns) + u128::from(carry.ns);
        let full = u64::try_from(scaled / NS_PER_S).unwrap_or(u64::MAX);
        carry.ns = u64::try_from(scaled % NS_PER_S).expect("remainder below 10^9");
        let floored = full.saturating_add(carry.floor);
        let floor = floored / FLOOR_DIVISOR;
        carry.floor = floored % FLOOR_DIVISOR;
        let net = full.saturating_sub(foreground);
        #[cfg(test)]
        {
            self.floor_bound += u64::from(net < floor);
        }
        net.max(floor)
    }
}

/// The checkpoint keep-up floor (ADR-0178 D2's keep-up grant, as its row states
/// it): the divisor α (none = no floor), and the one sub-unit remainder
/// the checkpoint's byte grant carries while the floor applies, in units
/// of `1 / (Σw × α)` — below `Σw × α`, so a u128.
#[derive(Copy, Clone, Debug, Default)]
struct KeepUp {
    alpha: Option<NonZeroU64>,
    carry: u128,
}

/// Which term set the checkpoint's byte grant on one refill.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum ByteGrant {
    /// The weighted share: every class without the floor, and the
    /// floored checkpoint when its share is the larger.
    Weighted,
    /// The keep-up term `foreground / α`, larger than the weighted share.
    KeepUp,
}

impl KeepUp {
    /// `max(grant × w / Σw, foreground / α)` compared exactly, then
    /// rounded once with the one remainder carried, so over any window
    /// the grants trail the sum of the exact per-refill maxima by under
    /// one unit (ADR-0178 D2). Each term is a whole part and a numerator below the
    /// denominator `Σw × α`, compared as a pair; every intermediate stays
    /// below 2⁶⁹ (`Σw ≤ 16`, `α < 2⁶⁴`), so nothing overflows.
    fn bytes(
        &mut self,
        alpha: NonZeroU64,
        grant: u64,
        foreground: u64,
        split: [u64; 2],
    ) -> (u64, ByteGrant) {
        let [weight, weights] = split.map(u128::from);
        if weights == 0 {
            return (0, ByteGrant::Weighted);
        }
        let alpha = u128::from(alpha.get());
        let weighted_num = u128::from(grant) * weight;
        let weighted = (weighted_num / weights, weighted_num % weights * alpha);
        let foreground = u128::from(foreground);
        // A planted canary: the keep-up term floored on its own, against
        // ADR-0178 D2's one carried remainder (the class oracle's crossover
        // regime and the crossover unit test must go red).
        let keepup_frac =
            if cfg!(inf_canary_keepup_truncates) { 0 } else { foreground % alpha * weights };
        let keepup = (foreground / alpha, keepup_frac);
        let (term, (whole, frac)) = if keepup > weighted {
            (ByteGrant::KeepUp, keepup)
        } else {
            (ByteGrant::Weighted, weighted)
        };
        let denominator = weights * alpha;
        let frac = frac + self.carry;
        let (whole, frac) =
            if frac >= denominator { (whole + 1, frac - denominator) } else { (whole, frac) };
        self.carry = frac;
        (u64::try_from(whole).unwrap_or(u64::MAX), term)
    }
}

/// One background class's grant for one refill (ADR-0178 D2): its
/// weighted share per axis, each axis with its own remainder; on the
/// checkpoint's byte axis with the keep-up floor on, [`KeepUp::bytes`],
/// and when the keep-up term sets it, an ops grant of at least
/// `⌈foreground / (α × slice)⌉`, so the ops axis never starves a floored
/// byte grant.
fn class_grant(
    class: IoClass,
    m: &mut Meter,
    grants: [u64; 2],
    weights: u64,
    foreground_bytes: u64,
    keepup: &mut KeepUp,
) -> ([u64; 2], ByteGrant) {
    let weight = class.weight();
    let ops = share(grants[OPS], weight, weights, &mut m.carry[OPS]);
    let alpha = match keepup.alpha {
        Some(alpha) if class == IoClass::Checkpoint => alpha,
        Some(_) | None => {
            let bytes = share(grants[BYTES], weight, weights, &mut m.carry[BYTES]);
            return ([bytes, ops], ByteGrant::Weighted);
        }
    };
    match keepup.bytes(alpha, grants[BYTES], foreground_bytes, [weight, weights]) {
        (bytes, ByteGrant::Weighted) => ([bytes, ops], ByteGrant::Weighted),
        (bytes, ByteGrant::KeepUp) => {
            let per_op = u128::from(alpha.get()) * u128::from(m.slice[BYTES].max(1));
            let floor = u128::from(foreground_bytes).div_ceil(per_op);
            let floor = u64::try_from(floor).unwrap_or(u64::MAX);
            ([bytes, ops.max(floor)], ByteGrant::KeepUp)
        }
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
    /// The checkpoint keep-up floor: the trigger's α and its remainder.
    keepup: KeepUp,
    /// The last background `Now` answer, until any other call.
    receipt: Option<Receipt>,
}

/// Per-class counters for INFO (ADR-0088 D7, ADR-0178 D5), per cell.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct ClassCounters {
    pub spent_bytes: u64,
    pub spent_ops: u64,
    /// Every `NotThisSlice`, attainable or not.
    pub deferrals: u64,
    /// Offers above the class cap on a budgeted axis, repeats included.
    pub unattainable: u64,
    /// Cumulative bytes offered above the cap by issued overruns (a
    /// refund does not reduce it; the live debt is the class's credit).
    pub overrun_bytes: u64,
}

impl DeviceBudget {
    /// `share` is the cell's share (`DeviceModel::share`); `slices`
    /// names each class's smallest offer (foreground entries are
    /// ignored); `checkpoint_alpha` is the trigger's α (the keep-up
    /// floor's divisor; 0 = no floor). `now` is the refill origin.
    #[must_use]
    pub fn new(
        share: DeviceModel,
        slices: [ClassSlice; IoClass::COUNT],
        checkpoint_alpha: u64,
        now: Nanos,
    ) -> Self {
        let mut write = Direction {
            rate: [share.write_bytes_per_s, share.write_ops_per_s],
            ..Direction::default()
        };
        let mut read = Direction {
            rate: [share.read_bytes_per_s, share.read_ops_per_s],
            ..Direction::default()
        };
        for class in IoClass::ALL {
            if class.is_foreground() {
                continue;
            }
            let dir = if class.is_read() { &mut read } else { &mut write };
            dir.weights += class.weight();
        }
        for dir in [&mut write, &mut read] {
            for axis in AXES {
                dir.pool_cap[axis] = horizon_of(dir.rate[axis], 1, 1);
            }
        }
        let mut meters = [Meter::default(); IoClass::COUNT];
        for class in IoClass::ALL {
            let m = &mut meters[class.index()];
            let slice = slices[class.index()];
            m.slice = [slice.bytes, slice.ops];
            if class.is_foreground() {
                continue;
            }
            let dir = if class.is_read() { &read } else { &write };
            m.cap[BYTES] =
                horizon_of(dir.rate[BYTES], class.weight(), dir.weights).max(slice.bytes);
            m.cap[OPS] =
                horizon_of(dir.rate[OPS], class.weight(), dir.weights).max(slice.ops.max(1));
            // Boot: every class holds its cap and is rested (ADR-0178 D2).
            m.credit = [Credit::Held(m.cap[BYTES]), Credit::Held(m.cap[OPS])];
        }
        DeviceBudget {
            write,
            read,
            meters,
            last_refill: now,
            model_absent: share.is_absent(),
            keepup: KeepUp { alpha: NonZeroU64::new(checkpoint_alpha), carry: 0 },
            receipt: None,
        }
    }

    /// True when no direction is budgeted (INFO `io_budget_model:absent`).
    #[must_use]
    pub const fn model_absent(&self) -> bool {
        self.model_absent
    }

    /// The cell's write/read byte shares per second (INFO).
    #[must_use]
    pub const fn share_bytes_per_s(&self) -> (u64, u64) {
        (self.write.rate[BYTES], self.read.rate[BYTES])
    }

    /// The class's cap (ADR-0178 D2).
    #[must_use]
    pub fn cap(&self, class: IoClass) -> ClassCap {
        let m = &self.meters[class.index()];
        ClassCap { bytes: m.cap[BYTES], ops: m.cap[OPS] }
    }

    /// Once per MAINTAIN entry (ADR-0178 D2). Time moving backwards or
    /// not at all is a no-op that keeps `last` (iteration-quantized
    /// clock).
    pub fn refill(&mut self, now: Nanos) {
        self.receipt = None;
        let elapsed = now.0.saturating_sub(self.last_refill.0);
        if elapsed == 0 {
            return;
        }
        self.last_refill = now;
        for is_read in [false, true] {
            self.refill_direction(is_read, elapsed);
        }
    }

    /// One direction's refill: the axis grants, split by weight (the
    /// checkpoint's keep-up floor on the write side), each class's refill
    /// row, and the overflow into the pool.
    fn refill_direction(&mut self, is_read: bool, elapsed: u64) {
        let dir = if is_read { &mut self.read } else { &mut self.write };
        let fg = std::mem::take(&mut dir.fg);
        if !dir.budgeted() {
            return;
        }
        let grants =
            [dir.axis_grant(BYTES, elapsed, fg[BYTES]), dir.axis_grant(OPS, elapsed, fg[OPS])];
        let mut overflow = [0u64; 2];
        for class in IoClass::ALL {
            if class.is_foreground() || class.is_read() != is_read {
                continue;
            }
            let m = &mut self.meters[class.index()];
            let (add, term) =
                class_grant(class, m, grants, dir.weights, fg[BYTES], &mut self.keepup);
            match term {
                ByteGrant::Weighted => {}
                #[cfg(test)]
                ByteGrant::KeepUp => dir.keepup_bound += 1,
                #[cfg(not(test))]
                ByteGrant::KeepUp => {}
            }
            refill_class(m, dir.rate, add, &mut overflow);
        }
        for axis in AXES {
            dir.pool[axis] = dir.pool[axis].saturating_add(overflow[axis]).min(dir.pool_cap[axis]);
        }
    }

    /// Offer `bytes`/`ops` for `class` (ADR-0178 D1): the one background
    /// entry point. Foreground classes and an unbudgeted direction are
    /// `Now` before any arithmetic (charged, counted). A background offer
    /// at most its class cap is granted from the credit then the pool, or
    /// is `NotThisSlice`; one above the cap goes to the overrun, which is
    /// `Now` when the class is `Rested` at its cap and `NotThisSlice`
    /// otherwise. Only `Now` moves the credit, the pool and `spent`.
    pub fn offer(&mut self, class: IoClass, bytes: u64, ops: u64) -> Issue {
        self.receipt = None;
        let request = [bytes, ops];
        if class.is_foreground() {
            self.meter_foreground(class, request);
            return Issue::Now;
        }
        if !self.direction(class).budgeted() {
            self.issue(class, request, [Draw::default(); 2]);
            return Issue::Now;
        }
        let issue = match self.admit(class, request) {
            Admission::Granted => Issue::Now,
            Admission::Deferred { short_bytes, short_ops } => {
                debug_assert!(short_bytes > 0 || short_ops > 0, "a deferral names its shortfall");
                Issue::NotThisSlice
            }
            Admission::Unattainable { cap_bytes, cap_ops } => {
                self.offer_above_cap(class, request, ClassCap { bytes: cap_bytes, ops: cap_ops })
            }
        };
        let m = &mut self.meters[class.index()];
        match issue {
            // A planted canary: a grant ends the rest pass, the rule
            // ADR-0178 rejects (the class oracle must see every overrun
            // behind a zero-work sibling starve).
            #[cfg(inf_canary_grant_clears_rest)]
            Issue::Now => m.rest = Rest::Filling,
            #[cfg(not(inf_canary_grant_clears_rest))]
            Issue::Now => {}
            Issue::NotThisSlice => m.deferrals += 1,
        }
        issue
    }

    /// An offer above the class cap (ADR-0178 D2): counted, then the
    /// overrun if the class is `Rested` at its cap.
    fn offer_above_cap(&mut self, class: IoClass, request: [u64; 2], cap: ClassCap) -> Issue {
        self.meters[class.index()].unattainable += 1;
        // A planted canary: the pre-overrun answer, "not this slice" for
        // ever (ADR-0170 R1–R3 and the sim arm must go red).
        if cfg!(inf_canary_unattainable_deferred) {
            return Issue::NotThisSlice;
        }
        let bytes_budgeted = self.direction(class).rate[BYTES] > 0;
        match self.admit_overrun(class, request) {
            Overrun::Granted { owed_bytes, owed_ops } => {
                let above_bytes =
                    if bytes_budgeted { request[BYTES].saturating_sub(cap.bytes) } else { 0 };
                // I6's local form; the planted any-held overrun breaks it on
                // purpose, so the class oracle must be what sees it.
                debug_assert!(
                    cfg!(inf_canary_overrun_any_held) || owed_bytes <= above_bytes,
                    "an overrun owes at most B − cap"
                );
                debug_assert!(
                    cfg!(inf_canary_overrun_any_held)
                        || owed_ops <= request[OPS].saturating_sub(cap.ops)
                );
                let m = &mut self.meters[class.index()];
                m.overrun_bytes = m.overrun_bytes.saturating_add(above_bytes);
                Issue::Now
            }
            Overrun::Deferred { short_bytes, short_ops } => {
                debug_assert!(
                    short_bytes > 0
                        || short_ops > 0
                        || self.meters[class.index()].rest == Rest::Filling,
                    "a refused overrun lacks credit or its rest pass"
                );
                Issue::NotThisSlice
            }
        }
    }

    /// The attainable half (ADR-0178 D2 table): unattainable above the
    /// cap on a budgeted axis; a debtor draws nothing; otherwise the
    /// class's credit, then the pool, or the exact shortfall.
    fn admit(&mut self, class: IoClass, request: [u64; 2]) -> Admission {
        let dir = if class.is_read() { &self.read } else { &self.write };
        let m = &self.meters[class.index()];
        let budgeted = |axis: usize| dir.rate[axis] > 0;
        if AXES.into_iter().any(|axis| budgeted(axis) && request[axis] > m.cap[axis]) {
            return Admission::Unattainable { cap_bytes: m.cap[BYTES], cap_ops: m.cap[OPS] };
        }
        let debtor = AXES.into_iter().any(|axis| budgeted(axis) && m.credit[axis].owed() > 0);
        let mut short = [0u64; 2];
        let mut draws = [Draw::default(); 2];
        for axis in AXES.into_iter().filter(|&axis| budgeted(axis)) {
            let pool = if debtor { 0 } else { dir.pool[axis] };
            let held = m.credit[axis].held();
            let owed = m.credit[axis].owed();
            short[axis] =
                request[axis].saturating_add(owed).saturating_sub(held.saturating_add(pool));
            let credit = request[axis].min(held);
            draws[axis] = Draw { credit, pool: request[axis] - credit, debt: 0 };
        }
        if short != [0, 0] {
            return Admission::Deferred { short_bytes: short[BYTES], short_ops: short[OPS] };
        }
        self.issue(class, request, draws);
        Admission::Granted
    }

    /// The overrun (ADR-0178 D2): only while the class is `Rested` and
    /// holds its cap on every budgeted axis. Per axis it draws what the
    /// class holds, then what the pool holds, and owes the rest.
    fn admit_overrun(&mut self, class: IoClass, request: [u64; 2]) -> Overrun {
        let dir = if class.is_read() { &self.read } else { &self.write };
        let m = &self.meters[class.index()];
        let budgeted = |axis: usize| dir.rate[axis] > 0;
        let mut short = [0u64; 2];
        let mut draws = [Draw::default(); 2];
        for axis in AXES.into_iter().filter(|&axis| budgeted(axis)) {
            let held = m.credit[axis].held();
            short[axis] = m.cap[axis].saturating_add(m.credit[axis].owed()).saturating_sub(held);
            let credit = request[axis].min(held);
            let above = request[axis] - credit;
            let pool = above.min(dir.pool[axis]);
            draws[axis] = Draw { credit, pool, debt: above - pool };
        }
        if !overrun_ready(m.rest, short, m.credit) {
            return Overrun::Deferred { short_bytes: short[BYTES], short_ops: short[OPS] };
        }
        self.issue(class, request, draws);
        Overrun::Granted { owed_bytes: draws[BYTES].debt, owed_ops: draws[OPS].debt }
    }

    /// Publishes a background `Now`: the draws leave the credit and the
    /// pool, `spent` counts the whole offer, and the receipt records it.
    fn issue(&mut self, class: IoClass, request: [u64; 2], draws: [Draw; 2]) {
        let dir = if class.is_read() { &mut self.read } else { &mut self.write };
        let m = &mut self.meters[class.index()];
        for axis in AXES {
            m.spent[axis] = m.spent[axis].saturating_add(request[axis]);
            if dir.rate[axis] == 0 {
                continue;
            }
            let draw = draws[axis];
            debug_assert!(draw.pool <= dir.pool[axis], "a grant draws only what the pool holds");
            dir.pool[axis] = dir.pool[axis].saturating_sub(draw.pool);
            let held = m.credit[axis].held().saturating_sub(draw.credit);
            m.credit[axis] =
                if draw.debt > 0 { Credit::owing(draw.debt) } else { Credit::Held(held) };
        }
        self.receipt = Some(Receipt { class, granted: request, draws });
    }

    /// Spend unconditionally (ADR-0178 D2): the op is issued whatever the
    /// credit says (a checkpoint header — its file is already created —
    /// its barriers, a tier round's bytes staged past its grant, or any
    /// foreground op). A background class owes what
    /// its credit cannot hold, repaid before its credit grows again; the
    /// counters stay exact. Never a deferral.
    pub fn charge(&mut self, class: IoClass, bytes: u64, ops: u64) {
        self.receipt = None;
        let request = [bytes, ops];
        if class.is_foreground() {
            self.meter_foreground(class, request);
            return;
        }
        let dir = if class.is_read() { &self.read } else { &self.write };
        let m = &mut self.meters[class.index()];
        for axis in AXES {
            m.spent[axis] = m.spent[axis].saturating_add(request[axis]);
            if dir.rate[axis] > 0 {
                m.credit[axis] = m.credit[axis].charged(request[axis]);
            }
        }
    }

    /// Return the unissued part of the grant just before this call (a
    /// tier round staged fewer bytes than its slice, a zero-fill found no
    /// slice, a cold read found the pool dry). The receipt caps it at
    /// that grant and returns it last-taken first — the debt, then the
    /// pool, then the credit — so a full refund is the grant's exact
    /// inverse and no refund lifts the credit or the pool above their
    /// values before it (ADR-0178 D2, I14/I15). A refund with no receipt
    /// of its class returns nothing. Foreground classes correct their
    /// counters only.
    pub fn refund(&mut self, class: IoClass, bytes: u64, ops: u64) {
        let receipt = self.receipt.take();
        let request = [bytes, ops];
        let dir = if class.is_read() { &mut self.read } else { &mut self.write };
        let m = &mut self.meters[class.index()];
        if class.is_foreground() {
            for axis in AXES {
                m.spent[axis] = m.spent[axis].saturating_sub(request[axis]);
                dir.fg[axis] = dir.fg[axis].saturating_sub(request[axis]);
            }
            return;
        }
        let Some(receipt) = receipt.filter(|receipt| receipt.class == class) else {
            debug_assert!(false, "a {class:?} refund must directly follow its own grant");
            return;
        };
        for axis in AXES {
            let returned = request[axis].min(receipt.granted[axis]);
            m.spent[axis] = m.spent[axis].saturating_sub(returned);
            if dir.rate[axis] == 0 {
                continue;
            }
            let draw = receipt.draws[axis];
            let to_debt = returned.min(draw.debt);
            let to_pool = (returned - to_debt).min(draw.pool);
            let to_credit = returned - to_debt - to_pool;
            debug_assert!(to_credit <= draw.credit, "a refund returns at most its grant's draws");
            m.credit[axis] = m.credit[axis].refunded(to_debt, to_credit, m.cap[axis]);
            dir.pool[axis] = dir.pool[axis].saturating_add(to_pool).min(dir.pool_cap[axis]);
        }
    }

    /// Per-class counters (INFO).
    #[must_use]
    pub fn counters(&self, class: IoClass) -> ClassCounters {
        let m = &self.meters[class.index()];
        ClassCounters {
            spent_bytes: m.spent[BYTES],
            spent_ops: m.spent[OPS],
            deferrals: m.deferrals,
            unattainable: m.unattainable,
            overrun_bytes: m.overrun_bytes,
        }
    }

    fn direction(&self, class: IoClass) -> &Direction {
        if class.is_read() { &self.read } else { &self.write }
    }

    /// A foreground op: counted, and subtracted from the next grant.
    fn meter_foreground(&mut self, class: IoClass, request: [u64; 2]) {
        let dir = if class.is_read() { &mut self.read } else { &mut self.write };
        let m = &mut self.meters[class.index()];
        for axis in AXES {
            dir.fg[axis] = dir.fg[axis].saturating_add(request[axis]);
            m.spent[axis] = m.spent[axis].saturating_add(request[axis]);
        }
    }
}

/// One class's refill row (ADR-0178 D2), per budgeted axis: the debt is
/// repaid first, the credit grows to its cap, the excess overflows to
/// the pool; `Rest` is `Rested` iff every budgeted axis held its cap
/// before the grant.
fn refill_class(m: &mut Meter, rate: [u64; 2], add: [u64; 2], overflow: &mut [u64; 2]) {
    let mut at_cap = true;
    for axis in AXES.into_iter().filter(|&axis| rate[axis] > 0) {
        at_cap &= m.credit[axis] == Credit::Held(m.cap[axis]);
        let (credit, excess) = m.credit[axis].refilled(add[axis], m.cap[axis]);
        m.credit[axis] = credit;
        overflow[axis] = overflow[axis].saturating_add(excess);
    }
    m.rest = if at_cap { Rest::Rested } else { Rest::Filling };
}

/// The overrun's precondition (ADR-0178 D2, I5): `Rested`, and the cap
/// held on every budgeted axis (`short` is what the class lacks to reach
/// it). The planted canary grants it from any held state instead — the
/// rule ADR-0178 rejects, which R4's same-class overrunner regime must
/// catch starving an attainable offer.
fn overrun_ready(rest: Rest, short: [u64; 2], credit: [Credit; 2]) -> bool {
    if cfg!(inf_canary_overrun_any_held) {
        return credit.iter().all(|axis| axis.owed() == 0);
    }
    rest == Rest::Rested && short == [0, 0]
}

/// `grant × weight / weights` with the remainder (mod `weights`) carried.
fn share(grant: u64, weight: u64, weights: u64, carry: &mut u64) -> u64 {
    if weights == 0 {
        return 0;
    }
    let scaled = u128::from(grant) * u128::from(weight) + u128::from(*carry);
    *carry = u64::try_from(scaled % u128::from(weights)).expect("remainder below weights");
    u64::try_from(scaled / u128::from(weights)).unwrap_or(u64::MAX)
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
        DeviceBudget::new(model().share(4), slices(), 0, Nanos(0))
    }

    /// What `class` holds on each axis (0 while it owes).
    fn held(b: &DeviceBudget, class: IoClass) -> (u64, u64) {
        let m = &b.meters[class.index()];
        (m.credit[BYTES].held(), m.credit[OPS].held())
    }

    /// The direction's shared pool.
    fn pool(b: &DeviceBudget, read: bool) -> (u64, u64) {
        let dir = if read { &b.read } else { &b.write };
        (dir.pool[BYTES], dir.pool[OPS])
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
        let mut b = DeviceBudget::new(DeviceModel::ABSENT, slices(), 2, Nanos(0));
        assert!(b.model_absent());
        for _ in 0..1_000 {
            assert_eq!(b.offer(IoClass::Checkpoint, 64 * MIB, 1), Issue::Now);
        }
        let c = b.counters(IoClass::Checkpoint);
        assert_eq!(c.spent_bytes, 64_000 * MIB);
        assert_eq!(c.spent_ops, 1_000);
        assert_eq!((c.deferrals, c.unattainable, c.overrun_bytes), (0, 0, 0));
        // An unbudgeted grant still has a receipt: its refund corrects the
        // counters, as a model-absent cold read's does.
        assert_eq!(b.offer(IoClass::ColdReadMaintain, 16 << 10, 1), Issue::Now);
        b.refund(IoClass::ColdReadMaintain, 12 << 10, 1);
        let c = b.counters(IoClass::ColdReadMaintain);
        assert_eq!((c.spent_bytes, c.spent_ops), (4 << 10, 0));
    }

    #[test]
    fn caps_are_one_burst_horizon_and_never_below_one_slice() {
        let b = budget();
        // Checkpoint: 100 MiB/s × 2/10 × 50 ms ≈ 1 MiB > its 256 KiB slice;
        // ops: 2000 × 2/10 × 50 ms = 20 ≥ the 1-op slice.
        let cap = b.cap(IoClass::Checkpoint);
        assert_eq!(cap, ClassCap { bytes: horizon_of(100 * MIB, 2, 10), ops: 20 });
        assert!(cap.bytes > 256 << 10);
        // Tier flush: 100 MiB/s × 4/10 × 50 ms ≈ 2 MiB — the horizon wins
        // over its 1 MiB slice; ops: 40 < its 256-op slice — the slice wins.
        let cap = b.cap(IoClass::TierFlush);
        assert_eq!(cap, ClassCap { bytes: horizon_of(100 * MIB, 4, 10), ops: 256 });
        // Boot: every class holds its cap.
        assert_eq!(held(&b, IoClass::TierFlush), (cap.bytes, cap.ops));
    }

    /// Offers at most the class cap: the class's own credit first, then
    /// the pool (work-conserving), then the exact shortfall.
    #[test]
    fn a_class_past_its_cap_overflows_into_the_shared_pool() {
        let mut b = budget();
        // Everything starts full; a second of idle refill goes entirely
        // to the pool, which caps at 50 ms of the share.
        b.refill(Nanos(1_000_000_000));
        let (pool_bytes, pool_ops) = pool(&b, false);
        assert_eq!(pool_bytes, horizon_of(100 * MIB, 1, 1));
        assert_eq!(pool_ops, 100);
        let cap = b.cap(IoClass::Checkpoint);
        // A busy class spends its credit with offers at its cap, then the
        // pool: the second cap-sized offer is the pool's.
        assert_eq!(b.offer(IoClass::Checkpoint, cap.bytes, 1), Issue::Now);
        assert_eq!(held(&b, IoClass::Checkpoint).0, 0);
        assert_eq!(b.offer(IoClass::Checkpoint, cap.bytes, 1), Issue::Now);
        assert_eq!(pool(&b, false).0, pool_bytes - cap.bytes);
        // The rest of the pool (4 MiB) is four more cap-sized offers.
        for _ in 0..4 {
            let take = pool(&b, false).0.min(cap.bytes);
            assert_eq!(b.offer(IoClass::Checkpoint, take, 1), Issue::Now);
        }
        assert_eq!(pool(&b, false).0, 0);
        // Nothing left: the exact shortfall is reported and counted.
        assert_eq!(
            b.admit(IoClass::Checkpoint, [4096, 1]),
            Admission::Deferred { short_bytes: 4096, short_ops: 0 }
        );
        assert_eq!(b.offer(IoClass::Checkpoint, 4096, 1), Issue::NotThisSlice);
        let c = b.counters(IoClass::Checkpoint);
        assert_eq!((c.deferrals, c.unattainable), (1, 0));
    }

    #[test]
    fn foreground_spend_reduces_the_grant_to_the_floor_and_no_further() {
        let mut b = budget();
        // Drain the checkpoint class first (the pool is empty at boot).
        let cap = b.cap(IoClass::Checkpoint);
        assert_eq!(b.offer(IoClass::Checkpoint, cap.bytes, 0), Issue::Now);
        assert_eq!(pool(&b, false).0, 0);
        // Foreground eats 10× the share over 10 ms; the grant clamps at
        // the floor (1/8 of 10 ms of share = 128 KiB), split by weight.
        assert_eq!(b.offer(IoClass::LogFrame, 10 * MIB, 100), Issue::Now);
        b.refill(Nanos(10_000_000));
        let ten_ms_share = 100 * MIB / 100;
        let floor = ten_ms_share / FLOOR_DIVISOR;
        let expected = mul_div(floor, 2, 10);
        assert_eq!(held(&b, IoClass::Checkpoint).0, expected);
        // With no foreground spend the full 10 ms share is granted.
        assert_eq!(b.offer(IoClass::Checkpoint, expected, 0), Issue::Now);
        b.refill(Nanos(20_000_000));
        assert_eq!(held(&b, IoClass::Checkpoint).0, mul_div(ten_ms_share, 2, 10));
        // The foreground is counted, never deferred.
        let fg = b.counters(IoClass::LogFrame);
        assert_eq!((fg.spent_bytes, fg.spent_ops, fg.deferrals), (10 * MIB, 100, 0));
    }

    /// The receipt (ADR-0178 D2): a refund returns at most the grant just
    /// before it, the debt first, then the pool, then the credit.
    #[test]
    fn a_refund_returns_at_most_its_grant_and_corrects_the_counters() {
        let mut b = budget();
        let cap = b.cap(IoClass::TierFlush);
        assert_eq!(b.offer(IoClass::TierFlush, MIB, 256), Issue::Now);
        assert_eq!(held(&b, IoClass::TierFlush).0, cap.bytes - MIB);
        // Staged only 300 KiB of the 1 MiB slice bound, 40 of 256 ops.
        b.refund(IoClass::TierFlush, MIB - (300 << 10), 216);
        assert_eq!(held(&b, IoClass::TierFlush), (cap.bytes - (300 << 10), 216));
        let c = b.counters(IoClass::TierFlush);
        assert_eq!((c.spent_bytes, c.spent_ops), (300 << 10, 40));
        // A refund larger than its grant returns the grant, no more.
        assert_eq!(b.offer(IoClass::TierFlush, MIB, 0), Issue::Now);
        b.refund(IoClass::TierFlush, 100 * MIB, 0);
        assert_eq!(held(&b, IoClass::TierFlush).0, cap.bytes - (300 << 10));
        assert_eq!(b.counters(IoClass::TierFlush).spent_bytes, 300 << 10);
    }

    /// A refund that does not directly follow its class's own grant
    /// returns nothing: the conservative side of the receipt's adjacency.
    #[test]
    #[should_panic(expected = "refund must directly follow its own grant")]
    fn a_refund_without_its_grant_is_a_caller_defect() {
        let mut b = budget();
        assert_eq!(b.offer(IoClass::TierFlush, MIB, 256), Issue::Now);
        b.refill(Nanos(1_000_000));
        b.refund(IoClass::TierFlush, MIB, 256);
    }

    /// `charge` owes what the credit cannot hold (ADR-0178 D2): the debt
    /// is repaid before the credit grows again, and a debtor is granted
    /// nothing — neither credit nor pool.
    #[test]
    fn an_unconditional_charge_owes_what_the_credit_cannot_hold() {
        let mut b = budget();
        let cap = b.cap(IoClass::Checkpoint);
        b.refill(Nanos(1_000_000_000)); // fills the pool
        b.charge(IoClass::Checkpoint, cap.bytes + 4096, 1);
        let m = &b.meters[IoClass::Checkpoint.index()];
        assert_eq!(m.credit[BYTES], Credit::owing(4096), "owed, never forgiven");
        assert_eq!(b.counters(IoClass::Checkpoint).spent_bytes, cap.bytes + 4096);
        assert!(pool(&b, false).0 >= 4096, "the pool could cover the offer");
        assert_eq!(
            b.offer(IoClass::Checkpoint, 4096, 1),
            Issue::NotThisSlice,
            "a debtor draws no pool"
        );
        // 1 ms of the checkpoint's share (≈ 20.9 KB) repays the 4 KiB first.
        b.refill(Nanos(1_001_000_000));
        let grant = mul_div(100 * MIB / 1000, 2, 10);
        assert_eq!(held(&b, IoClass::Checkpoint).0, grant - 4096);
    }

    /// The debtor's charge row (ADR-0178 D2): a charge to a class that
    /// already owes adds to its debt, and the refill repays the whole of
    /// it before the class holds a byte. A tier round that overran its cap
    /// and then staged past its offer reaches this row, as does a
    /// checkpoint header charged while the class still owes a block.
    #[test]
    fn a_charge_to_a_debtor_adds_to_its_debt() {
        let mut b = budget();
        let cap = b.cap(IoClass::TierFlush);
        let (above, excess) = (3 * MIB, MIB / 2);
        // From boot the class is rested at its cap and the pool is empty,
        // so the overrun owes everything above the cap.
        assert_eq!(b.offer(IoClass::TierFlush, cap.bytes + above, 1), Issue::Now);
        assert_eq!(b.meters[IoClass::TierFlush.index()].credit[BYTES], Credit::owing(above));
        b.charge(IoClass::TierFlush, excess, 0);
        let owed = b.meters[IoClass::TierFlush.index()].credit[BYTES];
        assert_eq!(owed, Credit::owing(above + excess), "added to the debt, never forgiven");
        assert_eq!(b.counters(IoClass::TierFlush).spent_bytes, cap.bytes + above + excess);
        // The tier share is 40 MiB/s: 87.5 ms grants exactly the 3.5 MiB
        // owed, so the class ends at zero — a forgiven charge would leave
        // it holding the half megabyte.
        b.refill(Nanos(87_500_000));
        assert_eq!(b.meters[IoClass::TierFlush.index()].credit[BYTES], Credit::Held(0));
    }

    /// The checkpoint keep-up floor: under foreground saturation the
    /// checkpoint class still receives `foreground bytes / α` per refill
    /// (and the ops to issue it), so it completes within α intervals.
    #[test]
    fn the_checkpoint_keeps_up_with_the_log_at_foreground_saturation() {
        let mut b = DeviceBudget::new(model().share(4), slices(), 2, Nanos(0));
        // Drain the checkpoint class and zero-fill (the pool is empty).
        for class in [IoClass::Checkpoint, IoClass::ZeroFill] {
            let cap = b.cap(class);
            assert_eq!(b.offer(class, cap.bytes, 0), Issue::Now);
        }
        // The log writes 10× the share in 10 ms: the weighted grant is the
        // floor (128 KiB × 2/10 = 25.6 KiB); the keep-up floor is 5 MiB.
        b.charge(IoClass::LogFrame, 10 * MIB, 100);
        b.refill(Nanos(10_000_000));
        let (bytes, ops) = held(&b, IoClass::Checkpoint);
        let cap = horizon_of(100 * MIB, 2, 10).max(256 << 10);
        assert_eq!(bytes, (5 * MIB).min(cap), "keep-up floor, capped at the class cap");
        assert!(ops >= 1);
        // Without α there is no floor: the weighted share alone.
        let mut plain = DeviceBudget::new(model().share(4), slices(), 0, Nanos(0));
        let cap = plain.cap(IoClass::Checkpoint);
        assert_eq!(plain.offer(IoClass::Checkpoint, cap.bytes, 0), Issue::Now);
        plain.charge(IoClass::LogFrame, 10 * MIB, 100);
        plain.refill(Nanos(10_000_000));
        assert_eq!(held(&plain, IoClass::Checkpoint).0, mul_div((100 * MIB / 100) / 8, 2, 10));
    }

    #[test]
    fn reads_and_writes_are_separate_directions() {
        let mut b = budget();
        let read_cap = b.cap(IoClass::ColdReadMaintain);
        assert_eq!(read_cap.bytes, horizon_of(50 * MIB, 1, 1).max(16 << 10));
        // A write-side drain leaves the read side untouched.
        let cap = b.cap(IoClass::ZeroFill);
        assert_eq!(b.offer(IoClass::ZeroFill, cap.bytes, 1), Issue::Now);
        assert_eq!(held(&b, IoClass::ColdReadMaintain).0, read_cap.bytes);
    }

    #[test]
    fn an_unbudgeted_dimension_never_defers_on_that_axis() {
        let m = DeviceModel { write_bytes_per_s: 100 * MIB, ..DeviceModel::ABSENT };
        let mut b = DeviceBudget::new(m, slices(), 0, Nanos(0));
        // The ops rate is 0: a million ops is neither unattainable nor
        // owed; bytes still bind.
        assert_eq!(b.offer(IoClass::Checkpoint, 4096, 1_000_000), Issue::Now);
        assert_eq!(b.counters(IoClass::Checkpoint).unattainable, 0);
        let left = held(&b, IoClass::Checkpoint).0;
        assert_eq!(
            b.admit(IoClass::Checkpoint, [left + 1, 0]),
            Admission::Deferred { short_bytes: 1, short_ops: 0 }
        );
    }

    #[test]
    fn refill_is_monotone_and_a_stalled_clock_is_a_no_op() {
        let mut b = budget();
        let cap = b.cap(IoClass::ZeroFill);
        assert_eq!(b.offer(IoClass::ZeroFill, cap.bytes, 1), Issue::Now);
        b.refill(Nanos(0));
        assert_eq!(held(&b, IoClass::ZeroFill).0, 0);
        b.refill(Nanos(5_000_000));
        let after = held(&b, IoClass::ZeroFill).0;
        assert_eq!(after, mul_div(100 * MIB / 200, 4, 10));
        // Backwards time: nothing moves.
        b.refill(Nanos(1_000_000));
        assert_eq!(held(&b, IoClass::ZeroFill).0, after);
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
            let bytes = (seed >> 16) % (8 * MIB);
            let ops = (seed >> 40) % 8;
            a.refill(Nanos(now));
            b.refill(Nanos(now));
            assert_eq!(a.offer(class, bytes, ops), b.offer(class, bytes, ops));
        }
        for class in IoClass::ALL {
            assert_eq!(a.counters(class), b.counters(class));
        }
    }

    /// M4.5-S39d's boundary checkpoint found the idle-loop starvation:
    /// with the reference box's probe (2 540 write ops/s per device, two
    /// cells) an idle loop iterating every ~430 µs granted
    /// `1270 × 0.00043 = 0.55 → 0` ops per refill, then `0 × 2/10 = 0` to
    /// the checkpoint class, forever — the keep-up floor that feeds it
    /// under load is zero at idle. Grants carry their sub-unit remainder
    /// across refills: one second of fast refills grants one second's
    /// ops, exactly, for every class.
    #[test]
    fn a_fast_idle_loop_still_grants_ops_through_the_carry() {
        let model = DeviceModel {
            write_bytes_per_s: 510_132_224,
            write_ops_per_s: 2_540,
            read_bytes_per_s: 0,
            read_ops_per_s: 0,
        };
        let mut b = DeviceBudget::new(model.share(2), slices(), 2, Nanos(0));
        // Drain every background write class's ops (a spent burst).
        for class in [IoClass::ZeroFill, IoClass::TierFlush, IoClass::Checkpoint] {
            let (_, ops) = held(&b, class);
            assert_eq!(b.offer(class, 0, ops), Issue::Now);
            assert_eq!(held(&b, class).1, 0);
        }
        let mut granted = 0u64;
        let mut now = 0u64;
        // 1 s of 430 µs refills, the checkpoint spending each op as soon
        // as it is granted (the stuck block re-offered every slice).
        while now < 1_000_000_000 {
            now += 430_000;
            b.refill(Nanos(now));
            let (_, ops) = held(&b, IoClass::Checkpoint);
            if ops > 0 {
                assert_eq!(b.offer(IoClass::Checkpoint, 0, ops), Issue::Now);
                granted += ops;
            }
        }
        // The class's share: 1270 × 2/10 = 254 ops/s (±1 for the carry).
        assert!((253..=255).contains(&granted), "checkpoint ops over 1 s: {granted}");
        // The other classes accrued to their caps, not to zero.
        assert!(held(&b, IoClass::ZeroFill).1 > 0);
        assert!(held(&b, IoClass::TierFlush).1 > 0);
    }

    /// R1 (ADR-0178): the 8-cell share of the 489 MB/s reference device
    /// is 61 125 000 B/s — a checkpoint cap of 611 250 B and a pool cap
    /// of 3 056 250 B. A 4 MiB section block is above both together, so
    /// before the overrun it was "not this slice" on every call. Built at
    /// t = 0 the class is rested at its cap and the pool is empty, so the
    /// overrun owes 4 194 304 − 611 250 B; a 256 KiB block after it is
    /// issued within `T_ckpt` of injected time (D4 with A1's three units
    /// of carry lag; α = 2: r = share / 7).
    #[test]
    fn a_checkpoint_block_above_the_class_cap_is_issued_within_its_bound() {
        let reference = DeviceModel {
            write_bytes_per_s: 489_000_000,
            write_ops_per_s: 320_000,
            read_bytes_per_s: 0,
            read_ops_per_s: 0,
        };
        let mut b = DeviceBudget::new(reference.share(8), slices(), 2, Nanos(0));
        assert_eq!(b.cap(IoClass::Checkpoint).bytes, 611_250);
        assert_eq!(b.write.pool_cap[BYTES], 3_056_250);
        assert_eq!(pool(&b, false), (0, 0));
        assert_eq!(b.offer(IoClass::Checkpoint, 4 * MIB, 1), Issue::Now, "the overrun issues");
        let c = b.counters(IoClass::Checkpoint);
        assert_eq!((c.unattainable, c.deferrals), (1, 0));
        assert_eq!(c.overrun_bytes, 3_583_054);
        let owed = b.meters[IoClass::Checkpoint.index()].credit[BYTES].owed();
        assert_eq!(owed, 3_583_054, "B − cap, with nothing in the pool");
        let units = owed + 611_250 + super::class_oracle::CARRY_LAG_UNITS;
        let bound_ns = units as f64 / (61_125_000.0 / 7.0) * 1e9 + 2e6;
        let mut step = 0u64;
        loop {
            step += 1;
            b.refill(Nanos(step * 1_000_000));
            if b.offer(IoClass::Checkpoint, 256 << 10, 1) == Issue::Now {
                break;
            }
            assert!(((step * 1_000_000) as f64) < bound_ns, "past T_ckpt at step {step}");
        }
        assert!(((step * 1_000_000) as f64) <= bound_ns, "issued at {step} ms");
    }

    /// ADR-0178 D4: near the crossover of the weighted
    /// share and the keep-up floor the checkpoint's credit still gains
    /// `share / 7` per second at α = 2, within three units over any
    /// window. The 8-cell reference share refilled every 10 µs grants
    /// 611.25 B a refill; a 175 B log charge sits just past the crossover
    /// (174.6 B), where the keep-up term alone is 87.5 B.
    #[test]
    fn the_keepup_floor_keeps_its_remainder_at_the_crossover() {
        const REFILLS: u64 = 5_000;
        const STEP_NS: u64 = 10_000;
        const LOG_BYTES_PER_REFILL: u64 = 175;
        let reference = DeviceModel {
            write_bytes_per_s: 489_000_000,
            write_ops_per_s: 320_000,
            read_bytes_per_s: 0,
            read_ops_per_s: 0,
        };
        let mut b = DeviceBudget::new(reference.share(8), slices(), 2, Nanos(0));
        let cap = b.cap(IoClass::Checkpoint);
        assert_eq!(b.offer(IoClass::Checkpoint, cap.bytes, 0), Issue::Now, "drain the class");
        assert_eq!(held(&b, IoClass::Checkpoint).0, 0);
        for step in 1..=REFILLS {
            b.charge(IoClass::LogFrame, LOG_BYTES_PER_REFILL, 0);
            b.refill(Nanos(step * STEP_NS));
        }
        let gained = held(&b, IoClass::Checkpoint).0;
        assert!(gained < cap.bytes, "the class stayed below its cap: {gained}");
        // share / 7 over 50 ms: 436 607 B, whole.
        let due = 61_125_000 * REFILLS * STEP_NS / (7 * 1_000_000_000);
        assert!(gained + 3 >= due, "gained {gained} B, due {due} B: lag {}", due - gained);
    }

    /// I14/I15 over random histories: a full refund is the grant's exact
    /// inverse from any state, and no refund lifts the credit or the pool
    /// above their values before the grant it reverses.
    #[test]
    fn a_refund_never_lifts_credit_or_pool_above_the_grant_it_reverses() {
        let mut b = DeviceBudget::new(model().share(4), slices(), 2, Nanos(0));
        let classes = [IoClass::ZeroFill, IoClass::TierFlush, IoClass::Checkpoint];
        let (mut seed, mut now, mut granted) = (0xB0D6_E7AA_5EED_0001u64, 0u64, 0u32);
        for _ in 0..20_000 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            now += seed % 2_000_000;
            b.refill(Nanos(now));
            let class = classes[(seed >> 8) as usize % classes.len()];
            let cap = b.cap(class);
            let bytes = (seed >> 16) % (cap.bytes * 3);
            let ops = (seed >> 40) % (cap.ops + 1);
            let before = (b.meters[class.index()], b.write);
            if b.offer(class, bytes, ops) == Issue::NotThisSlice {
                continue;
            }
            granted += 1;
            let full = seed % 3 == 0;
            let (refund_bytes, refund_ops) = if full {
                (bytes, ops)
            } else {
                ((seed >> 20) % (bytes + 1), (seed >> 50) % (ops + 1))
            };
            b.refund(class, refund_bytes, refund_ops);
            let (m, dir) = (&b.meters[class.index()], &b.write);
            for axis in AXES {
                assert!(m.credit[axis].held() <= before.0.credit[axis].held(), "I15 credit");
                assert!(dir.pool[axis] <= before.1.pool[axis], "I15 pool");
                if full {
                    assert_eq!(m.credit[axis], before.0.credit[axis], "I14 credit");
                    assert_eq!(dir.pool[axis], before.1.pool[axis], "I14 pool");
                    assert_eq!(m.spent[axis], before.0.spent[axis], "I14 spent");
                }
            }
            assert_eq!(m.rest, before.0.rest, "only refill writes Rest");
        }
        assert!(granted > 5_000, "the history granted and refunded ({granted})");
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

/// R4 — the class oracle (ADR-0178 D4, I2–I8, I12–I15). Every background
/// class of `IoClass::ALL`, so a new class joins without edits, under the
/// limits' hostile offers, five shares, three refill intervals and seven
/// regimes. The bound `T_c(B)` is computed from D4's formula — the
/// floored weighted share `r_c` per axis and D2's cap — never from budget
/// code, and the budget's caps are checked against that formula. I14 is
/// checked on every full refund (the zero-work sibling), I15 on a partial
/// refund of each of the case's grants, made on a copy of the budget.
#[cfg(test)]
mod class_oracle {
    use super::*;

    /// The trigger's α in every case: the checkpoint's keep-up floor is on.
    const ALPHA: u64 = 2;
    /// Byte shares per second: 1 B/s (horizon 0, cap = slice), the
    /// 489 MB/s reference device at 1, 8 and 256 cells, and 2⁶².
    const SHARES: [u64; 5] = [1, 489_000_000, 489_000_000 / 8, 489_000_000 / 256, 1 << 62];
    const FINE_NS: [u64; 3] = [1, 1_000_000, 1_000_000_000];
    const FINE_STEPS: u64 = 1_000;
    const COARSE_STEPS: u64 = 10_000;
    /// The coarse step is `⌈T_c / 9 800⌉`: the 9 897 forward steps among
    /// the 10 000 (every 97th is a zero-length or backwards step) then
    /// cover `T_c` plus 97 steps, so an offer still pending at the end has
    /// waited past its bound.
    const COARSE_DIVISOR: f64 = 9_800.0;
    const ZERO_STEP_EVERY: u64 = 97;
    const OFFERS_PER_CLASS: usize = 7;
    /// The carries keep every sub-unit remainder, so over any window the
    /// class's integer grant trails `r_c × window` by less than one unit
    /// per carry stage — the rate product, the ⅛ floor, and the weighted
    /// split (for the floored checkpoint, the one carry of the keep-up
    /// max): under 2⅛ units, so three whole units (ADR-0178 D4).
    pub(super) const CARRY_LAG_UNITS: u64 = 3;
    /// A case ends once its producer issued this many offers in the coarse
    /// phase (each wait checked) — the steady cycle then repeats — or at
    /// its last step. A starved offer therefore always runs to the end.
    const COARSE_ISSUES_TO_SETTLE: u32 = 3;
    /// The grid runs on at most this many scoped threads.
    const GRID_THREADS_MAX: usize = 8;

    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    enum Regime {
        Idle,
        /// Another class of the direction drains the pool every refill,
        /// visited before the case.
        PoolDrained,
        /// The foreground charges 10× the share between refills.
        Foreground,
        /// The foreground charges the checkpoint byte axis's crossover
        /// between refills (D4: `share × αw / (αw + Σw)` of the step),
        /// rounded up past it to `≡ α − 1 (mod α)`, so the keep-up term
        /// alone always leaves a remainder below one unit.
        Crossover,
        /// A same-class overrunner offers above the cap every pass.
        OverrunnerFirst,
        OverrunnerLast,
        /// A same-class sibling, visited first, granted and refunded in
        /// full every pass: `cap` on even passes, the largest offer on odd.
        ZeroWorkFirst,
    }

    const REGIMES: [Regime; 7] = [
        Regime::Idle,
        Regime::PoolDrained,
        Regime::Foreground,
        Regime::Crossover,
        Regime::OverrunnerFirst,
        Regime::OverrunnerLast,
        Regime::ZeroWorkFirst,
    ];

    /// How the case's producer re-offers: through `offer`, or — the
    /// harness's own canary — through the attainable half alone.
    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    enum Policy {
        Offer,
        AdmitOnly,
    }

    #[derive(Copy, Clone, Debug)]
    struct Case {
        class: IoClass,
        offer: [u64; 2],
        share: u64,
        fine_ns: u64,
        regime: Regime,
        policy: Policy,
    }

    #[derive(Debug, Default)]
    struct Engagement {
        cases: u64,
        pool_drained: u64,
        floor_bound: u64,
        keepup_bound: u64,
        overrun_delayed: u64,
        sibling_ahead: u64,
        /// Partial refunds of a grant that drew from two of debt, pool
        /// and credit — where the return order decides the result.
        refund_split: u64,
    }

    /// The production slices (`DurableCell::new`): zero-fill's 256 KiB
    /// fill, the tier's 1 MiB default slice in 256 ops, the checkpoint's
    /// section slice `ick_align_up(256 KiB + 16)`, one 16 KiB cold read.
    fn production_slices() -> [ClassSlice; IoClass::COUNT] {
        let mut s = [ClassSlice { bytes: 0, ops: 0 }; IoClass::COUNT];
        s[IoClass::ZeroFill.index()] = ClassSlice { bytes: 256 << 10, ops: 1 };
        s[IoClass::TierFlush.index()] = ClassSlice { bytes: 1 << 20, ops: 256 };
        s[IoClass::Checkpoint.index()] = ClassSlice { bytes: 266_240, ops: 1 };
        s[IoClass::ColdReadMaintain.index()] = ClassSlice { bytes: 16 << 10, ops: 1 };
        s
    }

    /// Each producer's largest offer (ADR-0170 D3's table), as the
    /// producers' constants state it: `ZERO_FILL_SLICE_BYTES`,
    /// `MAINTAIN-SLICE`'s 64 MiB ceiling,
    /// `ick_align_up(ICK_MAX_SECTION_BYTES + 13)` = 64 MiB + 8 KiB, and
    /// `COLD_POOL_BUF`.
    fn largest_offer(class: IoClass) -> [u64; 2] {
        let slice = production_slices()[class.index()];
        let bytes = match class {
            IoClass::ZeroFill => 256 << 10,
            IoClass::TierFlush => 64 << 20,
            IoClass::Checkpoint => (64 << 20) + (8 << 10),
            IoClass::ColdReadMaintain => 16 << 10,
            IoClass::LogFrame | IoClass::BlobWrite | IoClass::ColdReadForeground => 0,
        };
        [bytes, slice.ops]
    }

    fn model_of(share: u64) -> DeviceModel {
        let ops = (share / 4096).max(1);
        DeviceModel {
            write_bytes_per_s: share,
            write_ops_per_s: ops,
            read_bytes_per_s: share,
            read_ops_per_s: ops,
        }
    }

    fn background() -> impl Iterator<Item = IoClass> {
        IoClass::ALL.into_iter().filter(|class| !class.is_foreground())
    }

    /// The case's per-axis rate in the class's direction, from the model
    /// (the budget's share is the model it is given).
    fn rate_of(class: IoClass, share: u64) -> [u64; 2] {
        let model = model_of(share);
        if class.is_read() {
            [model.read_bytes_per_s, model.read_ops_per_s]
        } else {
            [model.write_bytes_per_s, model.write_ops_per_s]
        }
    }

    /// `rate × weight / weights × 50 ms`, floored once.
    fn horizon_units(rate: u64, weight: u64, weights: u64) -> u64 {
        let units = u128::from(rate) * u128::from(weight) * u128::from(BURST_HORIZON_NS)
            / (u128::from(weights) * NS_PER_S);
        u64::try_from(units).unwrap_or(u64::MAX)
    }

    /// D2's cap from its formula, not from budget code: `cap_c =
    /// max(slice_c, rate × w_c / Σw × 50 ms)`, at least 1 on the ops axis.
    fn d2_cap(class: IoClass, rate: [u64; 2]) -> [u64; 2] {
        let slice = production_slices()[class.index()];
        let weights: u64 = background()
            .filter(|other| other.is_read() == class.is_read())
            .map(IoClass::weight)
            .sum();
        let horizon = |axis: usize| horizon_units(rate[axis], class.weight(), weights);
        [slice.bytes.max(horizon(BYTES)), slice.ops.max(horizon(OPS)).max(1)]
    }

    /// D4's `r_c` per axis, per second: the ⅛ floor's weighted share; on
    /// the checkpoint's byte axis with α > 0, the larger of that and
    /// `w_c / (α w_c + Σw)`, where the keep-up floor crosses it.
    fn floor_rate(class: IoClass, rate: [u64; 2], axis: usize) -> f64 {
        let weights: u64 = background()
            .filter(|other| other.is_read() == class.is_read())
            .map(IoClass::weight)
            .sum();
        let (w, sum) = (class.weight() as f64, weights as f64);
        let floor = rate[axis] as f64 * w / (8.0 * sum);
        if class == IoClass::Checkpoint && axis == BYTES {
            let alpha = ALPHA as f64;
            floor.max(rate[axis] as f64 * w / (alpha * w + sum))
        } else {
            floor
        }
    }

    /// `max over budgeted axes of (O + cap + C + lag) / r_c`, in ns (C = 0:
    /// the oracle's producers charge nothing).
    fn wait_ns(class: IoClass, rate: [u64; 2], cap: [u64; 2], owed: [u64; 2]) -> f64 {
        AXES.into_iter()
            .filter(|&axis| rate[axis] > 0)
            .map(|axis| {
                let units = owed[axis] as f64 + cap[axis] as f64 + CARRY_LAG_UNITS as f64;
                units / floor_rate(class, rate, axis) * 1e9
            })
            .fold(0.0, f64::max)
    }

    /// The crossover regime's foreground charge for one step of `step_ns`
    /// at `rate` B/s: the least whole charge above `rate × step × αw / (αw
    /// + Σw)` (the checkpoint's weights) that is `≡ α − 1 (mod α)`.
    fn crossover(rate: u64, step_ns: u64) -> u64 {
        let w = u128::from(IoClass::Checkpoint.weight());
        let weights: u128 =
            background().filter(|c| !c.is_read()).map(|c| u128::from(c.weight())).sum();
        let alpha = u128::from(ALPHA);
        let num = u128::from(rate) * u128::from(step_ns) * alpha * w;
        let above = num / ((alpha * w + weights) * NS_PER_S) + 1;
        let odd = above + (alpha - 1 + alpha - above % alpha) % alpha;
        u64::try_from(odd).unwrap_or(u64::MAX)
    }

    /// The grid: every background class × its seven hostile offers × the
    /// shares × the intervals × the regimes.
    fn grid(policy: Policy, regimes: &[Regime]) -> Vec<Case> {
        let mut cases = Vec::new();
        for class in background() {
            for share in SHARES {
                let rate = rate_of(class, share);
                let cap = d2_cap(class, rate)[BYTES];
                let pool_cap = horizon_units(rate[BYTES], 1, 1);
                let ops = production_slices()[class.index()].ops;
                let bytes = [
                    1,
                    cap - 1,
                    cap,
                    cap + 1,
                    cap + pool_cap,
                    cap + pool_cap + 1,
                    largest_offer(class)[BYTES],
                ];
                debug_assert_eq!(bytes.len(), OFFERS_PER_CLASS);
                for offer in bytes {
                    for fine_ns in FINE_NS {
                        for &regime in regimes {
                            let offer = [offer, ops];
                            cases.push(Case { class, offer, share, fine_ns, regime, policy });
                        }
                    }
                }
            }
        }
        cases
    }

    /// The budget-visible state I14 compares across a grant and its full
    /// refund: the class's credit (debt included), `Rest` and `spent`,
    /// and the direction's pool. `spent` is compared only while the grant
    /// did not saturate it (its exhaustion policy, at `Meter::spent`).
    fn state_of(b: &DeviceBudget, class: IoClass) -> ([Credit; 2], Rest, [u64; 2], [u64; 2]) {
        let m = &b.meters[class.index()];
        (m.credit, m.rest, m.spent, b.direction(class).pool)
    }

    /// One case's run and its violations (empty when every check held).
    struct Run<'a> {
        case: Case,
        b: DeviceBudget,
        rate: [u64; 2],
        cap: [u64; 2],
        /// The case's largest offer per axis, over every producer in it.
        max_offer: [u64; 2],
        coarse_ns: u64,
        clock: u64,
        pass: u64,
        /// When the pending offer was first made, and what the class owed.
        pending: Option<(u64, [u64; 2])>,
        last_overrun_by_overrunner: bool,
        coarse_issues: u32,
        engagement: &'a mut Engagement,
        violations: Vec<String>,
    }

    impl Run<'_> {
        fn overrunner_offer(&self) -> [u64; 2] {
            // The class's largest offer, or cap + 1 where that is
            // attainable: the regime needs an overrun.
            let largest = largest_offer(self.case.class);
            [largest[BYTES].max(self.cap[BYTES] + 1), largest[OPS]]
        }

        fn sibling_offer(&self) -> [u64; 2] {
            if self.pass.is_multiple_of(2) {
                [self.cap[BYTES], largest_offer(self.case.class)[OPS]]
            } else {
                largest_offer(self.case.class)
            }
        }

        fn attainable(&self, offer: [u64; 2]) -> bool {
            AXES.into_iter().all(|axis| self.rate[axis] == 0 || offer[axis] <= self.cap[axis])
        }

        /// Whether D4 bounds the case's own offer in this regime: with one
        /// producer always; beside an overrunner only when attainable
        /// (I12); an overrunner's own wait is DV-1's.
        fn bounded(&self) -> bool {
            match self.case.regime {
                Regime::Idle
                | Regime::PoolDrained
                | Regime::Foreground
                | Regime::Crossover
                | Regime::ZeroWorkFirst => true,
                Regime::OverrunnerFirst | Regime::OverrunnerLast => {
                    self.attainable(self.case.offer)
                }
            }
        }

        fn bound_ns(&self, owed: [u64; 2]) -> f64 {
            let owed = match self.case.regime {
                // I12: with an overrunner, O is at most one overrun's debt.
                Regime::OverrunnerFirst | Regime::OverrunnerLast => self.owed_max(),
                Regime::Idle
                | Regime::PoolDrained
                | Regime::Foreground
                | Regime::Crossover
                | Regime::ZeroWorkFirst => owed,
            };
            wait_ns(self.case.class, self.rate, self.cap, owed) + 2.0 * self.coarse_ns as f64
        }

        /// I6's bound: `(maxoffer − cap)⁺` per axis (C = 0).
        fn owed_max(&self) -> [u64; 2] {
            [
                self.max_offer[BYTES].saturating_sub(self.cap[BYTES]),
                self.max_offer[OPS].saturating_sub(self.cap[OPS]),
            ]
        }

        /// Records the first violation of each kind (the text before its
        /// first `:`), so a planted rule shows every check it breaks.
        fn violation(&mut self, what: String) {
            let kind = what.split(':').next().unwrap_or_default().to_owned();
            if !self.violations.iter().any(|seen| seen.contains(&format!("}}: {kind}:"))) {
                self.violations.push(format!("{:?}: {what}", self.case));
            }
        }

        /// I3 (by type), I6 and I7 after every call.
        fn check_state(&mut self, after: &str) {
            let owed_max = self.owed_max();
            for class in background() {
                let m = &self.b.meters[class.index()];
                let dir = self.b.direction(class);
                for axis in AXES.into_iter().filter(|&axis| dir.rate[axis] > 0) {
                    let held_ok = m.credit[axis].held() <= m.cap[axis];
                    let pool_ok = dir.pool[axis] <= dir.pool_cap[axis];
                    let owed_ok =
                        class != self.case.class || m.credit[axis].owed() <= owed_max[axis];
                    if !(held_ok && pool_ok && owed_ok) {
                        let what = format!("I6/I7 after {after}: {class:?} axis {axis} {m:?}");
                        self.violation(what);
                        return;
                    }
                }
            }
        }

        /// One refill of the pass: the foreground's charge first in its
        /// regime; every 97th pass a zero-length or backwards refill.
        fn refill(&mut self) {
            let step = if self.pass < FINE_STEPS { self.case.fine_ns } else { self.coarse_ns };
            let now = if (self.pass + 1).is_multiple_of(ZERO_STEP_EVERY) {
                if (self.pass / ZERO_STEP_EVERY).is_multiple_of(2) {
                    self.clock
                } else {
                    self.clock.saturating_sub(step / 2 + 1)
                }
            } else {
                self.clock += step;
                self.charge_foreground(step);
                self.clock
            };
            let mut before = [false; IoClass::COUNT];
            for class in background() {
                before[class.index()] = self.at_cap(class);
            }
            let elapsed = now.saturating_sub(self.b.last_refill.0) > 0;
            self.b.refill(Nanos(now));
            for class in background() {
                let was_at_cap = before[class.index()];
                let rest = self.b.meters[class.index()].rest;
                let budgeted = self.b.direction(class).budgeted();
                if elapsed && budgeted && (rest == Rest::Rested) != was_at_cap {
                    self.violation(format!("I13: {class:?} at cap {was_at_cap} but {rest:?}"));
                }
            }
            self.check_state("refill");
        }

        /// The foreground's charge before a forward refill, in the two
        /// foreground regimes.
        fn charge_foreground(&mut self, step: u64) {
            let fg = if self.case.class.is_read() {
                IoClass::ColdReadForeground
            } else {
                IoClass::LogFrame
            };
            let ten = |rate: u64| {
                u64::try_from(u128::from(rate) * 10 * u128::from(step) / NS_PER_S)
                    .unwrap_or(u64::MAX)
            };
            match self.case.regime {
                Regime::Foreground => self.b.charge(fg, ten(self.rate[BYTES]), ten(self.rate[OPS])),
                Regime::Crossover => self.b.charge(fg, crossover(self.rate[BYTES], step), 0),
                Regime::Idle
                | Regime::PoolDrained
                | Regime::OverrunnerFirst
                | Regime::OverrunnerLast
                | Regime::ZeroWorkFirst => {}
            }
        }

        fn at_cap(&self, class: IoClass) -> bool {
            let m = &self.b.meters[class.index()];
            let dir = self.b.direction(class);
            AXES.into_iter()
                .filter(|&axis| dir.rate[axis] > 0)
                .all(|axis| m.credit[axis] == Credit::Held(m.cap[axis]))
        }

        /// The pool-drain competitor: another class of the direction takes
        /// what its own credit plus the pool can give, up to its cap.
        fn drain_pool(&mut self) {
            let class = self.case.class;
            let Some(rival) = background().find(|c| *c != class && c.is_read() == class.is_read())
            else {
                return;
            };
            let had = self.b.direction(class).pool[BYTES];
            for _ in 0..16 {
                let pool = self.b.direction(class).pool;
                let m = &self.b.meters[rival.index()];
                let take = [
                    m.cap[BYTES].min(m.credit[BYTES].held().saturating_add(pool[BYTES])),
                    m.cap[OPS].min(m.credit[OPS].held().saturating_add(pool[OPS])),
                ];
                if pool == [0, 0] || take == [0, 0] {
                    break;
                }
                if self.b.offer(rival, take[BYTES], take[OPS]) == Issue::NotThisSlice {
                    break;
                }
                self.check_state("pool drain");
            }
            if had > 0 && self.b.direction(class).pool[BYTES] == 0 {
                self.engagement.pool_drained += 1;
            }
        }

        fn overrunner(&mut self) {
            let offer = self.overrunner_offer();
            if self.b.offer(self.case.class, offer[BYTES], offer[OPS]) == Issue::Now {
                self.last_overrun_by_overrunner = true;
            }
            self.check_state("overrunner");
        }

        /// The zero-work sibling: granted, then refunded in full; I14
        /// compares the budget before its offer with the budget after.
        fn zero_work_sibling(&mut self) {
            let class = self.case.class;
            let offer = self.sibling_offer();
            let mut before = state_of(&self.b, class);
            let saturates =
                AXES.into_iter().any(|axis| before.2[axis].checked_add(offer[axis]).is_none());
            if self.b.offer(class, offer[BYTES], offer[OPS]) == Issue::Now {
                if self.pending.is_some() {
                    self.engagement.sibling_ahead += 1;
                }
                self.b.refund(class, offer[BYTES], offer[OPS]);
                let mut after = state_of(&self.b, class);
                if saturates {
                    (before.2, after.2) = ([0; 2], [0; 2]);
                }
                if after != before {
                    self.violation(format!("I14: {before:?} became {after:?}"));
                }
            }
            self.check_state("zero-work sibling");
        }
    }

    impl Run<'_> {
        /// The case's producer: offers, checks I2, I4, I5 and I13 against
        /// the state before, and its wait against `T_c(B)` on `Now`.
        fn producer(&mut self) {
            let class = self.case.class;
            let [bytes, ops] = self.case.offer;
            let m = self.b.meters[class.index()];
            let dir_before = self.b.direction(class).pool;
            if self.pending.is_none() {
                let owed = [m.credit[BYTES].owed(), m.credit[OPS].owed()];
                self.pending = Some((self.clock, owed));
            }
            let issue = match self.case.policy {
                Policy::Offer => self.b.offer(class, bytes, ops),
                Policy::AdmitOnly => match self.b.admit(class, [bytes, ops]) {
                    Admission::Granted => Issue::Now,
                    Admission::Deferred { .. } | Admission::Unattainable { .. } => {
                        Issue::NotThisSlice
                    }
                },
            };
            let after = self.b.meters[class.index()];
            let attainable = self.attainable(self.case.offer);
            let debtor = AXES.into_iter().any(|axis| m.credit[axis].owed() > 0);
            let rested_at_cap = m.rest == Rest::Rested && self.at_cap_of(&m);
            if self.case.policy == Policy::Offer {
                let counted = after.unattainable - m.unattainable;
                if counted != u64::from(!attainable) {
                    self.violation(format!("I2: unattainable counted {counted}"));
                }
                if rested_at_cap && issue != Issue::Now {
                    self.violation("I13: a rested class at its cap refused".to_owned());
                }
            }
            if issue == Issue::NotThisSlice
                && (after.credit != m.credit || self.b.direction(class).pool != dir_before)
            {
                self.violation("I2: a refused offer moved the credit or the pool".to_owned());
            }
            if debtor && issue == Issue::Now {
                self.violation("I4: a debtor was granted".to_owned());
            }
            if !attainable && issue == Issue::Now && !rested_at_cap {
                self.violation("I5: an overrun outside Rested at the cap".to_owned());
            }
            if issue == Issue::NotThisSlice && debtor && self.last_overrun_by_overrunner {
                self.engagement.overrun_delayed += 1;
            }
            if issue == Issue::Now {
                self.last_overrun_by_overrunner = false;
                self.coarse_issues += u32::from(self.pass >= FINE_STEPS);
                self.settle_wait();
                if self.case.policy == Policy::Offer {
                    self.partial_refund(&m, dir_before);
                }
            }
            self.check_state("producer");
        }

        /// I15 on a partial refund of each of the case's grants (a tier
        /// round that staged half its slice), made on a copy of the budget
        /// so the run's timing is the unrefunded one: half of what was
        /// granted returns last-taken first — the debt, then the pool,
        /// then the credit — computed here from the draws the grant made,
        /// and neither the credit nor the pool rises above its value
        /// before the grant.
        fn partial_refund(&mut self, before: &Meter, pool_before: [u64; 2]) {
            let class = self.case.class;
            let refund = [self.case.offer[BYTES] / 2, self.case.offer[OPS] / 2];
            let granted = self.b.meters[class.index()];
            let pool_granted = self.b.direction(class).pool;
            let (mut credit_due, mut pool_due) = (granted.credit, pool_granted);
            for axis in AXES.into_iter().filter(|&axis| self.rate[axis] > 0) {
                let debt = granted.credit[axis].owed();
                // A grant only draws; one that raised either is caught below.
                let pool = pool_before[axis].saturating_sub(pool_granted[axis]);
                let credit = before.credit[axis].held().saturating_sub(granted.credit[axis].held());
                let to_debt = refund[axis].min(debt);
                let to_pool = (refund[axis] - to_debt).min(pool);
                let to_credit = refund[axis] - to_debt - to_pool;
                let sources = [debt, pool, credit].into_iter().filter(|&draw| draw > 0).count();
                self.engagement.refund_split += u64::from(sources > 1 && refund[axis] > 0);
                credit_due[axis] = if debt > to_debt {
                    Credit::owing(debt - to_debt)
                } else {
                    Credit::Held(granted.credit[axis].held() + to_credit)
                };
                pool_due[axis] = pool_granted[axis] + to_pool;
            }
            let mut copy = self.b.clone();
            copy.refund(class, refund[BYTES], refund[OPS]);
            let (m, pool) = (copy.meters[class.index()], copy.direction(class).pool);
            if m.credit != credit_due || pool != pool_due {
                let what = format!("{:?} {pool:?}, due {credit_due:?} {pool_due:?}", m.credit);
                self.violation(format!("I15: a partial refund returned {what}"));
            }
            let lifted = AXES.into_iter().filter(|&axis| self.rate[axis] > 0).any(|axis| {
                m.credit[axis].held() > before.credit[axis].held() || pool[axis] > pool_before[axis]
            });
            if lifted {
                self.violation("I15: a refund lifted the credit or the pool".to_owned());
            }
            for axis in AXES.into_iter().filter(|&axis| granted.spent[axis] < u64::MAX) {
                if m.spent[axis] != granted.spent[axis] - refund[axis] {
                    self.violation(format!("I15: spent {:?} after refunding {refund:?}", m.spent));
                }
            }
        }

        fn at_cap_of(&self, m: &Meter) -> bool {
            let dir = self.b.direction(self.case.class);
            AXES.into_iter()
                .filter(|&axis| dir.rate[axis] > 0)
                .all(|axis| m.credit[axis] == Credit::Held(m.cap[axis]))
        }

        /// I8/I12/I14: an issued offer waited at most `T_c(B)`.
        fn settle_wait(&mut self) {
            let Some((since, owed)) = self.pending.take() else { return };
            let waited = self.clock - since;
            let bound = self.bound_ns(owed);
            if self.bounded() && waited as f64 > bound {
                self.violation(format!(
                    "I8/I12: waited {waited} ns past T_c {bound:.0} ns (owed {owed:?})"
                ));
            }
        }

        fn run(mut self) -> Vec<String> {
            for pass in 0..FINE_STEPS + COARSE_STEPS {
                self.pass = pass;
                self.refill();
                match self.case.regime {
                    Regime::PoolDrained => self.drain_pool(),
                    Regime::OverrunnerFirst => self.overrunner(),
                    Regime::ZeroWorkFirst => self.zero_work_sibling(),
                    Regime::Idle
                    | Regime::Foreground
                    | Regime::Crossover
                    | Regime::OverrunnerLast => {}
                }
                self.producer();
                if self.case.regime == Regime::OverrunnerLast {
                    self.overrunner();
                }
                if self.coarse_issues >= COARSE_ISSUES_TO_SETTLE {
                    break;
                }
            }
            if let Some((since, owed)) = self.pending {
                let bound = self.bound_ns(owed);
                let waited = self.clock - since;
                if self.bounded() && waited as f64 > bound {
                    self.violation(format!(
                        "I8/I12 starved: pending {waited} ns, T_c {bound:.0} ns"
                    ));
                }
            }
            let dir = self.b.direction(self.case.class);
            match self.case.regime {
                Regime::Foreground => self.engagement.floor_bound += u64::from(dir.floor_bound > 0),
                Regime::Crossover => {
                    self.engagement.keepup_bound += u64::from(dir.keepup_bound > 0)
                }
                Regime::Idle
                | Regime::PoolDrained
                | Regime::OverrunnerFirst
                | Regime::OverrunnerLast
                | Regime::ZeroWorkFirst => {}
            }
            self.violations
        }
    }

    fn run_case(case: Case, engagement: &mut Engagement) -> Vec<String> {
        let b = DeviceBudget::new(model_of(case.share), production_slices(), ALPHA, Nanos(0));
        let rate = rate_of(case.class, case.share);
        let cap = d2_cap(case.class, rate);
        let mut run = Run {
            case,
            b,
            rate,
            cap,
            max_offer: case.offer,
            coarse_ns: 0,
            clock: 0,
            pass: 0,
            pending: None,
            last_overrun_by_overrunner: false,
            coarse_issues: 0,
            engagement,
            violations: Vec::new(),
        };
        let other = match case.regime {
            Regime::OverrunnerFirst | Regime::OverrunnerLast => run.overrunner_offer(),
            Regime::ZeroWorkFirst => {
                let largest = largest_offer(case.class);
                [largest[BYTES].max(cap[BYTES]), largest[OPS]]
            }
            Regime::Idle | Regime::PoolDrained | Regime::Foreground | Regime::Crossover => [0, 0],
        };
        run.max_offer = [case.offer[BYTES].max(other[BYTES]), case.offer[OPS].max(other[OPS])];
        let longest = wait_ns(case.class, rate, cap, run.owed_max());
        run.coarse_ns = case.fine_ns.max((longest / COARSE_DIVISOR).ceil() as u64);
        run.engagement.cases += 1;
        for class in background() {
            let (built, due) =
                (run.b.meters[class.index()].cap, d2_cap(class, rate_of(class, case.share)));
            if built != due {
                run.violation(format!("cap: {class:?} built {built:?}, D2's formula {due:?}"));
            }
        }
        run.run()
    }

    /// Runs every case on up to `GRID_THREADS_MAX` scoped threads; the
    /// engagement counters and the violations are summed.
    fn run_grid(cases: &[Case]) -> (Engagement, Vec<String>) {
        let threads = std::thread::available_parallelism().map_or(1, usize::from);
        let chunk = cases.len().div_ceil(threads.clamp(1, GRID_THREADS_MAX)).max(1);
        let parts: Vec<(Engagement, Vec<String>)> = std::thread::scope(|scope| {
            let handles: Vec<_> = cases
                .chunks(chunk)
                .map(|part| {
                    scope.spawn(move || {
                        let mut engagement = Engagement::default();
                        let mut failures = Vec::new();
                        for &case in part {
                            failures.extend(run_case(case, &mut engagement));
                        }
                        (engagement, failures)
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().expect("a grid thread panicked")).collect()
        });
        let mut total = Engagement::default();
        let mut failures = Vec::new();
        for (engagement, part) in parts {
            total.cases += engagement.cases;
            total.pool_drained += engagement.pool_drained;
            total.floor_bound += engagement.floor_bound;
            total.keepup_bound += engagement.keepup_bound;
            total.overrun_delayed += engagement.overrun_delayed;
            total.sibling_ahead += engagement.sibling_ahead;
            total.refund_split += engagement.refund_split;
            failures.extend(part);
        }
        (total, failures)
    }

    /// R4: no background offer waits past its bound, over the whole grid,
    /// with every check after every call and every regime engaged.
    #[test]
    fn no_background_offer_waits_past_its_bound() {
        let cases = grid(Policy::Offer, &REGIMES);
        let classes = background().count();
        let expected = classes * OFFERS_PER_CLASS * SHARES.len() * FINE_NS.len() * REGIMES.len();
        assert_eq!(cases.len(), expected, "the grid is its product");
        assert_eq!(expected, 2_940);
        let (engagement, failures) = run_grid(&cases);
        assert_eq!(engagement.cases, 2_940, "every case ran");
        assert!(failures.is_empty(), "{} cases broke:\n{}", failures.len(), failures.join("\n"));
        assert!(engagement.pool_drained > 0, "vacuous: the pool was never drained");
        assert!(engagement.floor_bound > 0, "vacuous: the foreground never cut to the floor");
        assert!(engagement.keepup_bound > 0, "vacuous: the keep-up term never set a grant");
        assert!(engagement.overrun_delayed > 0, "vacuous: no overrun delayed an offer");
        assert!(engagement.sibling_ahead > 0, "vacuous: the zero-work sibling never went first");
        assert!(engagement.refund_split > 0, "vacuous: no partial refund met a split grant");
    }

    /// R4's own canary: a producer that re-offers through the attainable
    /// half alone never issues an offer above its cap — the pre-overrun
    /// wedge — and the harness must report every such case starved.
    #[test]
    fn the_class_oracle_sees_an_admit_only_producer_starve_above_the_cap() {
        let mut above_cap = 0usize;
        for case in grid(Policy::AdmitOnly, &[Regime::Idle]) {
            let b = DeviceBudget::new(model_of(case.share), production_slices(), ALPHA, Nanos(0));
            let cap = b.cap(case.class);
            let attainable = case.offer[BYTES] <= cap.bytes && case.offer[OPS] <= cap.ops;
            let (_, violations) = run_grid(&[case]);
            if attainable {
                assert!(violations.is_empty(), "{violations:?}");
            } else {
                above_cap += 1;
                assert!(
                    violations.iter().any(|v| v.contains("starved")),
                    "{case:?}: an admit-only producer above the cap was not reported starved"
                );
            }
        }
        assert!(above_cap >= 4 * SHARES.len() * FINE_NS.len(), "cases above the cap: {above_cap}");
    }

    /// The class's credit, the direction's pool and the class's `spent`.
    fn moved(b: &DeviceBudget, class: IoClass) -> ([Credit; 2], [u64; 2], [u64; 2]) {
        let m = &b.meters[class.index()];
        (m.credit, b.direction(class).pool, m.spent)
    }

    /// R4's single calls at 0 and the integer maximum, which no producer
    /// offers (I1, I2). Per background class and share: 0 is `Now` from a
    /// class that owes nothing and never unattainable, even from a debtor;
    /// `u64::MAX` counts unattainable once per call, moves nothing when
    /// refused, and when the class is rested at its cap issues with
    /// `spent` and `overrun_bytes` saturating.
    #[test]
    fn zero_and_the_integer_maximum_on_single_calls() {
        for share in SHARES {
            for class in background() {
                let b = DeviceBudget::new(model_of(share), production_slices(), ALPHA, Nanos(0));
                integer_extremes_of_one_class(b, class);
            }
        }
    }

    fn integer_extremes_of_one_class(mut b: DeviceBudget, class: IoClass) {
        let tag = format!("{class:?} at {} B/s", b.direction(class).rate[BYTES]);
        let cap = b.cap(class);
        let counted = |b: &DeviceBudget| {
            let c = b.counters(class);
            (c.unattainable, c.deferrals, c.overrun_bytes)
        };
        assert_eq!(b.offer(class, 0, 0), Issue::Now, "{tag}");
        assert_eq!(counted(&b), (0, 0, 0), "{tag}: 0 is never unattainable");
        // Rested at its cap: the overrun issues, and `spent` saturates.
        assert_eq!(b.offer(class, u64::MAX, 1), Issue::Now, "{tag}");
        assert_eq!(counted(&b), (1, 0, u64::MAX - cap.bytes), "{tag}");
        assert_eq!(b.counters(class).spent_bytes, u64::MAX, "{tag}");
        // Its full refund is its exact inverse; `Rest` is refill's.
        b.refund(class, u64::MAX, 1);
        assert_eq!(
            b.meters[class.index()].credit,
            [Credit::Held(cap.bytes), Credit::Held(cap.ops)]
        );
        assert_eq!(b.counters(class).spent_bytes, 0, "{tag}");
        // A second overrun saturates `overrun_bytes`.
        assert_eq!(b.offer(class, u64::MAX, u64::MAX), Issue::Now, "{tag}");
        assert_eq!(counted(&b), (2, 0, u64::MAX), "{tag}: overrun bytes saturate");
        // Now a debtor: the maximum is refused and moves nothing, 0 too.
        let before = moved(&b, class);
        assert_eq!(b.offer(class, u64::MAX, 1), Issue::NotThisSlice, "{tag}");
        assert_eq!(counted(&b), (3, 1, u64::MAX), "{tag}: counted once");
        assert_eq!(b.offer(class, 0, 0), Issue::NotThisSlice, "{tag}: a debtor draws nothing");
        assert_eq!(counted(&b), (3, 2, u64::MAX), "{tag}: 0 is never unattainable");
        assert_eq!(moved(&b, class), before, "{tag}: a refusal moved something");
        b.charge(class, u64::MAX, u64::MAX);
        assert_eq!(b.counters(class).spent_bytes, u64::MAX, "{tag}: spent saturates");
        // Below its cap and owing nothing: refused, counted once, unmoved.
        let mut b = DeviceBudget::new(
            model_of(b.direction(class).rate[BYTES]),
            production_slices(),
            ALPHA,
            Nanos(0),
        );
        assert_eq!(b.offer(class, cap.bytes, cap.ops), Issue::Now, "{tag}: drain");
        let before = moved(&b, class);
        assert_eq!(b.offer(class, u64::MAX, 1), Issue::NotThisSlice, "{tag}");
        assert_eq!(counted(&b), (1, 1, 0), "{tag}");
        assert_eq!(moved(&b, class), before, "{tag}: a refusal moved something");
        assert_eq!(b.offer(class, 0, 0), Issue::Now, "{tag}: 0 from an empty class");
    }

    /// I1 at the extremes: foreground classes on a budgeted model, and
    /// every class on an absent one, answer `Now` for 0 and the integer
    /// maximum and count nothing unattainable; a refill after a saturated
    /// foreground charge (the keep-up term at `u64::MAX / α`) is total.
    #[test]
    fn foreground_and_absent_model_offers_are_now_at_any_size() {
        for share in SHARES {
            let mut b = DeviceBudget::new(model_of(share), production_slices(), ALPHA, Nanos(0));
            for class in IoClass::ALL.into_iter().filter(|c| c.is_foreground()) {
                for size in [0, u64::MAX] {
                    assert_eq!(b.offer(class, size, size), Issue::Now, "{class:?}");
                }
                assert_eq!(b.counters(class).unattainable, 0, "{class:?}");
                assert_eq!(b.counters(class).spent_bytes, u64::MAX, "{class:?}");
            }
            b.refill(Nanos(1_000_000));
            let ckpt = b.meters[IoClass::Checkpoint.index()];
            assert_eq!(ckpt.credit[BYTES], Credit::Held(ckpt.cap[BYTES]), "{share} B/s");
        }
        let mut b = DeviceBudget::new(DeviceModel::ABSENT, production_slices(), ALPHA, Nanos(0));
        for class in IoClass::ALL {
            for size in [0, u64::MAX] {
                assert_eq!(b.offer(class, size, size), Issue::Now, "{class:?}");
            }
            assert_eq!(b.counters(class).unattainable, 0, "{class:?}");
        }
    }
}
