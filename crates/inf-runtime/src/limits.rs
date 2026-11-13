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
