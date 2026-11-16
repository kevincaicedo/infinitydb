//! `inf-runtime` limits: one `const` per bound, with its unit, its owner
//! and what a caller observes when it is crossed (INFINITY_STYLE "Put a
//! limit on everything").

use inf_foundation::bounded::{Cap, CapFill};

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
/// which [`ColdTurn::begin`](crate::ColdTurn::begin) alone builds — at a native iteration's first
/// lend, by the reactor, or once a declared turn, by a standalone host — and which every delivery
/// call of the turn shares; each native lend charges what its call spent to the turn's Foreground
/// group. Crossing: pace — the members not yet visited wait for the next turn, which the park
/// decision keeps coming while any is ready; nothing is dropped.
pub const COLD_DELIVERIES_PER_TURN: u32 = 64;

/// Work units one host turn's cold-read preparation spends (ADR-0181 R3): 64, a head inspection
/// one unit and any other candidate two. Owner: the turn's
/// [`ColdPreparationBudget`](crate::ColdPreparationBudget), which
/// [`ColdTurn::begin`](crate::ColdTurn::begin) alone builds, beside the delivery allowance, and
/// which no caller can reset. Crossing: none yet — a turn builds it beside the delivery
/// allowance, and no drain spends it until the budgeted preparation pass, whose crossing is pace,
/// the queued requests not yet inspected waiting for the next turn.
pub const COLD_PREPARATION_WORK_PER_TURN: u32 = 64;

/// Canary: the delivery turns after a receipt's own that a planted host lets pass before it
/// visits its last ready member, 7: phase 7e's first wait step at its 256 readers, one past
/// its round's bound of ⌈256 / 64⌉ + 2 steps. The plant in `cold` and the scenario's assertion
/// read this one const, so a moved delay fails the planted build.
#[cfg(inf_canary_cold_ready_left_seven_turns)]
pub const COLD_READY_LEFT_TURNS: u64 = 7;
/// Canary: the same, 15: the last step of phase 7e's shortest wait, its first step and the
/// held reads' 8 steady steps past the bound.
#[cfg(inf_canary_cold_ready_left_fifteen_turns)]
pub const COLD_READY_LEFT_TURNS: u64 = 15;

/// Task classes one cell's executor registers (ADR-0177 D2): 16. Owner: the
/// executor's layout ([`ExecutorLayout`](crate::ExecutorLayout)), which a cell
/// builds before it serves. A cell's plane registers six; tests and the
/// simulator's assemblies fewer. Crossing: `CapError::Layout` from
/// `ExecutorLayout::class`, before the executor is built, so the cell does
/// not serve.
pub const TASK_CLASSES_MAX: usize = 16;

/// Holders one cell's fabric gate may number (ADR-0177 D4): 2^24, the key's
/// index field. Owner: the fabric gate, whose word adapter is built from its
/// holder slab. Crossing: a typed refusal at boot — a plane whose `H` passes
/// it is `CapError::Layout` before the cell serves.
pub const GATE_HOLDERS_MAX: u32 = 1 << inf_foundation::limits::GATE_KEY_INDEX_BITS;

/// The fabric gate's holder slab (ADR-0177 D4): its ceiling, under which a
/// cell's `H = Σ T_j × W_j + (cells − 1) × d` is derived. Census name
/// `fabric-gate-holders`, filled while serving. Unit: holders per cell.
/// Owner: the fabric gate. Crossing: pace — a send that finds every holder
/// taken parks on the gate's room until one frees (derived never: units bound
/// the waiting holders and the peers' credits the orphans).
pub const GATE_HOLDERS: Cap =
    Cap::entries::<GATE_HOLDERS_MAX>("fabric-gate-holders", CapFill::Serving);
