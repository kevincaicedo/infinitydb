//! `inf-runtime` limits: one `const` per bound, with its unit, its owner
//! and what a caller observes when it is crossed (INFINITY_STYLE "Put a
//! limit on everything").

/// Timer callbacks a cell's loop begins in one turn (ADR-0154 D3): 64.
/// Owner: [`CellLoop`](crate::CellLoop), which in step 1 begins each with
/// [`TimerSet::begin`](crate::TimerSet::begin), settles it with
/// [`TimerSet::consume`](crate::TimerSet::consume) before the next, and
/// charges each to its turn's Maintenance group; the turn begins at most
/// the lesser of this and that group's budget. Crossing: pace — a timer
/// still due waits for the next turn, which delivers it before any later
/// deadline (the heap's order), and the park decision polls meanwhile;
/// nothing is dropped and no callback begins twice.
pub const TIMER_DELIVERIES_PER_TURN: u32 = 64;

/// The longest park a cell loop can be configured with (ADR-0003 A1.2.7): 100 ms. Every park a
/// [`CellLoop`](crate::CellLoop) issues carries a timeout of at most its loop's
/// [`ParkCeiling`](crate::ParkCeiling), and the ceiling is at most this: a wake no doorbell
/// rings — a waker used off its cell's thread — is drained by the iteration that park opens.
/// Owner: [`ParkCeiling::new`](crate::ParkCeiling::new), the ceiling's one constructor.
/// Crossing: a typed refusal — `ParkCeiling::new` answers
/// [`ParkCeilingTooLong`](crate::ParkCeilingTooLong) above it, and `infinityd` refuses a
/// `--park-us` above it at argument parsing, before any cell starts.
pub const PARK_CEILING_MAX: core::time::Duration = core::time::Duration::from_millis(100);

/// Identity pairs one cold-read host issues in its lifetime (ADR-0181 R4): 2^55. Pair `n` is
/// the logical id `2n` and the device id `2n + 1`, each a word index of the 56-bit `TierRead`
/// token payload, so the payload holds 2^55 pairs. Owner: the host's
/// [`PairIssuer`](crate::PairIssuer), which mints one pair at each admitted `enqueue` and never
/// mints a pair twice. Crossing: a typed, permanent refusal — `enqueue` answers
/// [`ColdRefused::IdentityExhausted`](crate::ColdRefused::IdentityExhausted) before any queue,
/// gate or pin changes, every later request is refused alike, and the reads admitted before it
/// still issue and deliver. A namespace drop does not reset the issuer; only a new host does.
pub const COLD_IDENTITY_PAIRS_MAX: u64 = 1 << 55;

/// Member visits one host turn's cold-read delivery makes (ADR-0181 R1): 64, live, cancelled or
/// orphaned members alike. Owner: the turn's [`ColdDeliveryBudget`](crate::ColdDeliveryBudget),
/// which [`ColdTurn::begin`](crate::ColdTurn::begin) alone builds — once a native iteration, by
/// the reactor, or once a declared turn, by a standalone host — and which every delivery call of
/// the turn shares; the reactor charges what it spent to the turn's Foreground group. Crossing:
/// pace — the members not yet visited wait for the next turn, which the park decision keeps
/// coming while any is ready; nothing is dropped.
pub const COLD_DELIVERIES_PER_TURN: u32 = 64;

/// Work units one host turn's cold-read preparation spends (ADR-0181 R3): 64, a head inspection
/// one unit and any other candidate two. Owner: the turn's
/// [`ColdPreparationBudget`](crate::ColdPreparationBudget), which
/// [`ColdTurn::begin`](crate::ColdTurn::begin) alone builds, beside the delivery allowance, and
/// which no caller can reset. Crossing: pace — the queued requests not yet inspected wait for
/// the next turn.
pub const COLD_PREPARATION_WORK_PER_TURN: u32 = 64;
