//! `inf-runtime` limits: one `const` per bound, with its unit, its owner
//! and what a caller observes when it is crossed (INFINITY_STYLE "Put a
//! limit on everything").

/// Timer callbacks a [`TimerSet`](crate::TimerSet)'s holder begins in one
/// turn (ADR-0154 D3): 64. Owner: the set's holder, which begins each with
/// [`TimerSet::begin`](crate::TimerSet::begin), settles it with
/// [`TimerSet::consume`](crate::TimerSet::consume) before the next, and
/// charges each to its turn's Maintenance group. Crossing: pace — a timer
/// still due waits for the next turn, which delivers it before any later
/// deadline (the heap's order); nothing is dropped and no callback begins
/// twice.
pub const TIMER_DELIVERIES_PER_TURN: u32 = 64;
