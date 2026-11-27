//! The extent-seal window's queue and turns (ADR-0163 D2; ADR-0061 D3). The
//! window's units, one per extent barrier registered and not folded, are the
//! ledger's (`GroupCommit::register_extent`); this module decides which blob
//! writer may reserve one, and in what order.
//!
//! A blob writer with no turn is an arrival. [`admit`] lets it reserve only
//! while the window is open and no writer is parked, and, with turns out,
//! only when a unit is free beyond theirs; otherwise it parks at the queue's
//! tail holding a place, never a unit. The window closes at the first LOG
//! step that holds the staged frame for an outstanding extent barrier and
//! opens once that frame has sealed (or nothing is staged); at the opening
//! the LOG step hands out `min(16 − len − w, q)` turns from the queue's head.
//! A woken writer resolves its key once more and reserves with its turn,
//! open or closed: every turn has a unit (`len + w ≤ 16`). A fold frees
//! units and wakes no one. There is no retry on the window: a woken writer
//! publishes or leaves, and a turn that leaves gives its unit back, to the
//! queue's head while the window is open and to the next handout while it
//! is closed. So no frame waits for an extent registered after its hold
//! began, beyond the turns already out, and position `q` is woken within
//! `⌈q / 16⌉` turnovers.
//!
//! The counts are exact. `q`, the writers parked, is the wait list's own
//! count of queued waits: it moves at the poll that queues a writer's wait
//! and at that wait's wake or unlink, and the queue keeps no count of its
//! own. `w` counts the turns out, wakes landed and not yet settled. A turn
//! resolves only on a landed wake: a wait whose first poll found no node in
//! the cell's pool is replaced, never read as a wake. A turn settles its own
//! count once, at its publish, its refusal or its drop. Only a drop passes a
//! unit on.

use core::cell::Cell;
use core::future::Future;
use core::mem;
use core::pin::Pin;
use core::task::{Context, Poll, Waker};
use std::rc::Rc;

use inf_log::limits::EXTENT_SEAL_WINDOW_ENTRIES;
use inf_runtime::gate::ListWait;
use inf_runtime::{WaitList, WaitNodes};

/// The window's units, as the queue counts them.
const ENTRIES: usize = EXTENT_SEAL_WINDOW_ENTRIES as usize;

/// What [`admit`] answers an arrival: a blob writer with no turn.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub(crate) enum Arrival {
    /// Reserve a unit. With no turn out the window's own reservation is
    /// the capacity answer, and at the cap its `Full` is the row's crossing.
    Reserve,
    /// Join the queue's tail.
    Park,
}

/// The arrival test (`len + w ≤ 16`): an arrival reserves only while the
/// window is `open` and no writer is `parked` ahead of it, and, with
/// `turns_out`, only while a unit is free beyond the `len` entries and the
/// turns.
pub(crate) fn admit(open: bool, parked: usize, len: usize, turns_out: usize) -> Arrival {
    #[cfg(not(inf_canary_extent_window_arrival_blind))]
    {
        if !open || parked > 0 {
            return Arrival::Park;
        }
        if turns_out == 0 || len.saturating_add(turns_out) < ENTRIES {
            Arrival::Reserve
        } else {
            Arrival::Park
        }
    }
    // Canary: while the window is open every arrival reserves, past the
    // turns out and the writers parked.
    #[cfg(inf_canary_extent_window_arrival_blind)]
    {
        let _ = (parked, len, turns_out);
        if open { Arrival::Reserve } else { Arrival::Park }
    }
}

/// The queue's state, shared with every turn so a turn settles its own
/// count when it drops.
struct Queue {
    open: Cell<bool>,
    /// `w`: turns out — writers woken and not yet published or gone.
    turns_out: Cell<usize>,
    /// The writers' waits; its own count of queued waits is `q`.
    list: WaitList<()>,
}

impl Queue {
    /// Wakes the queue's head, if a writer is queued: its place becomes a
    /// turn out. A writer whose wait is on no list is not reached. Returns
    /// whether a wake landed.
    fn wake_head(&self) -> bool {
        if self.list.is_idle() {
            return false;
        }
        let landed = self.list.wake_one(());
        debug_assert!(landed, "the list's count of queued waits holds to its links");
        if landed {
            self.turns_out.set(self.turns_out.get().saturating_add(1));
        }
        landed
    }

    /// `q`: the writers whose waits are queued.
    fn parked(&self) -> usize {
        self.list.waiting()
    }
}

/// The extent-seal window's queue (ADR-0163 D2): one per cell, in the
/// durable cell beside the staging wait list.
pub(crate) struct ExtentWaiters {
    queue: Rc<Queue>,
    /// Park episodes (cell scope): every writer that joined the queue.
    parks: u64,
    /// Turns that found the window full (cell scope); `len + w ≤ 16` makes
    /// it 0.
    turn_refused: u64,
}

impl ExtentWaiters {
    /// An open window with nobody queued, its list on the cell's `nodes`.
    pub(crate) fn new(nodes: &WaitNodes) -> ExtentWaiters {
        let queue =
            Queue { open: Cell::new(true), turns_out: Cell::new(0), list: WaitList::unit(nodes) };
        ExtentWaiters { queue: Rc::new(queue), parks: 0, turn_refused: 0 }
    }

    /// The arrival test for a writer with no turn, against the window's
    /// `len` entries.
    pub(crate) fn arrival(&self, len: usize) -> Arrival {
        let queue = &self.queue;
        admit(queue.open.get(), queue.parked(), len, queue.turns_out.get())
    }

    /// Parks a writer at the queue's tail: one park episode. The turn's wait
    /// is made on no list; the poll that queues it counts the writer in `q`.
    /// The writer awaits the turn, which resolves once a handout wakes it.
    pub(crate) fn park(&mut self) -> ExtentTurn {
        let queue = &self.queue;
        self.parks = self.parks.saturating_add(1);
        ExtentTurn { queue: Rc::clone(queue), state: TurnState::Waiting(queue.list.wait(())) }
    }

    /// A turn found the window full, which `len + w ≤ 16` rules out:
    /// debug-asserted, and counted, so the release form reads the count.
    /// The turn settles and passes nothing on, since it found no unit to
    /// give back, and the writer parks again at the tail for a later
    /// handout.
    pub(crate) fn refuse_turn(&mut self, turn: ExtentTurn) -> ExtentTurn {
        debug_assert!(false, "a turn found the extent-seal window full (len + w ≤ 16 broken)");
        self.turn_refused = self.turn_refused.saturating_add(1);
        turn.settle_refused();
        self.park()
    }

    /// The LOG step held the staged frame for an outstanding extent
    /// barrier: the window closes, and no arrival reserves until the frame
    /// has sealed. Returns whether it was open — one hold episode.
    pub(crate) fn close(&self) -> bool {
        self.queue.open.replace(false)
    }

    /// The staged frame sealed, or nothing is staged: the window opens and
    /// hands out `min(16 − len − w, q)` turns from the queue's head, at most
    /// the window's units whatever the number parked. `len` is the
    /// window's entries.
    pub(crate) fn hand_out(&self, len: usize) {
        let queue = &self.queue;
        queue.open.set(true);
        let turns_out = queue.turns_out.get();
        debug_assert!(
            len.saturating_add(turns_out) <= ENTRIES,
            "every turn has a unit (len + w ≤ 16)"
        );
        let free = ENTRIES.saturating_sub(len).saturating_sub(turns_out);
        #[cfg(not(any(inf_canary_extent_window_no_wake, inf_canary_extent_window_wake_all)))]
        let wakes = free.min(queue.parked());
        // Canary: the handout skips its wakes.
        #[cfg(inf_canary_extent_window_no_wake)]
        let wakes = {
            let _ = free;
            0
        };
        // Canary: the handout wakes every parked writer.
        #[cfg(inf_canary_extent_window_wake_all)]
        let wakes = {
            let _ = free;
            queue.parked()
        };
        for _ in 0..wakes {
            if !queue.wake_head() {
                break;
            }
        }
    }

    /// Writers parked now (`q`, cell scope).
    pub(crate) fn parked(&self) -> usize {
        self.queue.parked()
    }

    /// Turns out now (`w`, cell scope).
    pub(crate) fn turns_out(&self) -> usize {
        self.queue.turns_out.get()
    }

    /// Park episodes (cell scope).
    pub(crate) fn parks(&self) -> u64 {
        self.parks
    }

    /// Turns that found the window full (cell scope).
    pub(crate) fn turn_refused(&self) -> u64 {
        self.turn_refused
    }
}

/// A turn's phase: it resolves only on a landed wake, and settles once.
enum TurnState {
    /// Its wait, queued or not yet: the turn has not resolved.
    Waiting(ListWait<()>),
    /// Its wake landed and it resolved: it holds no wait, and is a turn out
    /// until it settles.
    Landed,
    /// Its count settled: it settles nothing again.
    Settled,
}

/// A blob writer's place in the window's order, from its park until it
/// publishes or leaves (ADR-0163 D2): a place, not a unit — a writer whose
/// wake has landed resolves its key once more and reserves with its turn.
/// Awaited, it resolves once its wake has landed. Dropped, it settles its
/// own count: a landed wake gives its unit back (one wake passes it to the
/// queue's head while the window is open), an unlanded one leaves the queue
/// in place.
#[must_use = "a turn is a place in the window's order; dropping it leaves the queue"]
pub(crate) struct ExtentTurn {
    queue: Rc<Queue>,
    state: TurnState,
}

impl ExtentTurn {
    /// The writer published its extent with this turn: the unit is the
    /// window's entry now (`w − 1`), and nothing is passed on.
    pub(crate) fn spend(mut self) {
        let landed = self.settle();
        debug_assert!(landed, "a turn is spent only after its wake landed");
    }

    /// The turn found the window full: `w − 1`, and nothing is passed on.
    /// It had no unit to give back, so a wake would send the queue's head
    /// into the same full window.
    fn settle_refused(mut self) {
        let landed = self.settle();
        debug_assert!(landed, "a turn reserves only after its wake landed");
    }

    /// Settles the turn's own count, once. A landed turn was a turn out
    /// (`w − 1`, with no poll). A waiting one is polled once: Ready is a
    /// wake landed unseen (`w − 1`); Pending drops the wait, and a queued
    /// one unlinks (`q − 1`, by the list). Returns whether a wake had
    /// landed; a settled turn settles nothing again.
    fn settle(&mut self) -> bool {
        let queue = &*self.queue;
        match mem::replace(&mut self.state, TurnState::Settled) {
            TurnState::Waiting(mut wait) => {
                let landed = landed(&mut wait);
                if landed {
                    queue.turns_out.set(queue.turns_out.get().saturating_sub(1));
                }
                // An unlanded wait unlinks at its drop; a landed one was
                // polled done.
                drop(wait);
                landed
            }
            TurnState::Landed => {
                queue.turns_out.set(queue.turns_out.get().saturating_sub(1));
                true
            }
            TurnState::Settled => false,
        }
    }
}

/// One poll of a turn's wait: whether its wake has landed. A turn's wait is
/// never left starved between polls (its poll replaces one), so a Ready here
/// is a landed wake. A landed wait is left done, so dropping it passes
/// nothing on.
fn landed(wait: &mut ListWait<()>) -> bool {
    let mut cx = Context::from_waker(Waker::noop());
    Pin::new(wait).poll(&mut cx).is_ready()
}

impl Future for ExtentTurn {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = Pin::into_inner(self);
        match &mut this.state {
            TurnState::Waiting(wait) => match Pin::new(&mut *wait).poll(cx) {
                Poll::Ready(()) => {
                    this.state = TurnState::Landed;
                    Poll::Ready(())
                }
                Poll::Pending => {
                    // A starved wait woke its task and would answer Ready
                    // with no wake: it is replaced by a new one, not polled,
                    // so a Ready here is always a landed wake.
                    if wait.starved() {
                        *wait = this.queue.list.wait(());
                    }
                    Poll::Pending
                }
            },
            TurnState::Landed | TurnState::Settled => Poll::Ready(()),
        }
    }
}

impl Drop for ExtentTurn {
    /// A turn that leaves gives its unit back: one wake passes it to the
    /// queue's head while the window is open; while it is closed the next
    /// handout counts it free.
    fn drop(&mut self) {
        if self.settle() && self.queue.open.get() {
            self.queue.wake_head();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `admit`'s table (`len + w ≤ 16`): every arrival behind a parked
    /// writer or into a closed window parks; with turns out an arrival
    /// reserves only a unit free beyond theirs; with none, the window
    /// answers.
    #[test]
    fn admit_parks_behind_the_queue_and_never_takes_a_turns_unit() {
        let rows = [
            // (open, q, len, w) → answer
            ((true, 0, 0, 0), Arrival::Reserve),
            ((true, 0, 16, 0), Arrival::Reserve),
            ((true, 0, 15, 0), Arrival::Reserve),
            ((true, 1, 0, 0), Arrival::Park),
            ((false, 0, 0, 0), Arrival::Park),
            ((false, 0, 0, 3), Arrival::Park),
            ((true, 0, 0, 15), Arrival::Reserve),
            ((true, 0, 0, 16), Arrival::Park),
            ((true, 0, 5, 10), Arrival::Reserve),
            ((true, 0, 5, 11), Arrival::Park),
            ((true, 3, 0, 1), Arrival::Park),
        ];
        for ((open, parked, len, turns_out), answer) in rows {
            assert_eq!(
                admit(open, parked, len, turns_out),
                answer,
                "open {open}, q {parked}, len {len}, w {turns_out}"
            );
        }
    }

    fn poll_turn(turn: &mut ExtentTurn) -> bool {
        let mut cx = Context::from_waker(Waker::noop());
        Pin::new(turn).poll(&mut cx).is_ready()
    }

    /// An open window on a census and a pool of its own.
    fn window() -> ExtentWaiters {
        let (census, _mark) = inf_foundation::bounded::CapCensus::new(inf_foundation::CellId(0));
        let nodes =
            WaitNodes::build(crate::limits::TEST_WAIT_NODES, &census).expect("a pool").nodes;
        ExtentWaiters::new(&nodes)
    }

    /// A writer parked and its turn polled once, as the writer's task polls
    /// the turn in the step that parks it: not yet woken.
    fn parked(waiters: &mut ExtentWaiters) -> ExtentTurn {
        let mut turn = waiters.park();
        assert!(!poll_turn(&mut turn), "engagement: a parked turn is not woken");
        turn
    }

    /// A handout wakes `min(16 − len − w, q)` writers from the head and no
    /// more, whatever the number parked; the woken are turns out.
    #[test]
    fn a_handout_wakes_at_most_the_free_units_from_the_head() {
        let mut waiters = window();
        assert!(waiters.close(), "the window starts open");
        let mut turns: Vec<ExtentTurn> = (0..40).map(|_| parked(&mut waiters)).collect();
        assert_eq!((waiters.parked(), waiters.parks()), (40, 40));
        waiters.hand_out(4);
        assert_eq!((waiters.parked(), waiters.turns_out()), (28, 12));
        let woken: Vec<bool> = turns.iter_mut().map(poll_turn).collect();
        assert!(woken[..12].iter().all(|&w| w), "the head is woken first");
        assert!(woken[12..].iter().all(|&w| !w), "no wake past the free units");
        waiters.hand_out(4);
        assert_eq!(waiters.turns_out(), 12, "a unit is never handed out twice");
    }

    /// A turn that leaves settles its own count: a landed one gives its unit
    /// to the queue's head while the window is open and to the next handout
    /// while it is closed; an unlanded one leaves the queue in place.
    #[test]
    fn a_leaving_turn_settles_its_count_and_passes_its_unit_on() {
        let mut waiters = window();
        let mut turns: Vec<ExtentTurn> = (0..3).map(|_| parked(&mut waiters)).collect();
        waiters.hand_out(15);
        assert_eq!((waiters.parked(), waiters.turns_out()), (2, 1));
        let first = turns.remove(0);
        drop(first);
        assert_eq!((waiters.parked(), waiters.turns_out()), (1, 1), "the unit passed on");
        assert!(poll_turn(&mut turns[0]), "to the queue's head");
        waiters.close();
        let woken = turns.remove(0);
        drop(woken);
        assert_eq!((waiters.parked(), waiters.turns_out()), (1, 0), "closed: kept for a handout");
        let parked = turns.remove(0);
        drop(parked);
        assert_eq!((waiters.parked(), waiters.turns_out()), (0, 0), "an unlanded turn leaves");
    }

    /// A spent turn is the window's entry now: `w − 1`, nothing passed on.
    #[test]
    fn a_spent_turn_passes_nothing_on() {
        let mut waiters = window();
        let mut turns: Vec<ExtentTurn> = (0..2).map(|_| parked(&mut waiters)).collect();
        waiters.hand_out(15);
        let mut first = turns.remove(0);
        assert!(poll_turn(&mut first));
        first.spend();
        assert_eq!((waiters.parked(), waiters.turns_out()), (1, 0));
        assert!(!poll_turn(&mut turns[0]), "the spent unit went to the window, not the queue");
    }

    /// A refused turn settles its own count and wakes no one: it found no
    /// unit to give back, so the queue's head stays parked for a later
    /// handout instead of resolving into the same full window.
    #[test]
    fn a_refused_turn_settles_its_count_and_wakes_no_one() {
        let mut waiters = window();
        let mut turns: Vec<ExtentTurn> = (0..2).map(|_| parked(&mut waiters)).collect();
        waiters.hand_out(15);
        let mut refused = turns.remove(0);
        assert!(poll_turn(&mut refused), "the head's wake landed");
        assert_eq!((waiters.parked(), waiters.turns_out()), (1, 1));
        refused.settle_refused();
        assert_eq!((waiters.parked(), waiters.turns_out()), (1, 0), "w − 1, the queue in place");
        assert!(!poll_turn(&mut turns[0]), "no head woken into the full window");
    }

    /// A window on a pool of `cap` nodes, and the pool.
    fn window_on(cap: inf_foundation::bounded::Cap) -> (ExtentWaiters, WaitNodes) {
        let (census, _mark) = inf_foundation::bounded::CapCensus::new(inf_foundation::CellId(0));
        let nodes = WaitNodes::build(cap, &census).expect("a pool").nodes;
        (ExtentWaiters::new(&nodes), nodes)
    }

    const ONE_NODE: inf_foundation::bounded::Cap = inf_foundation::bounded::Cap::entries::<1>(
        "test-one-node",
        inf_foundation::bounded::CapFill::Serving,
    );

    /// A turn on an empty pool (ADR-0177 D5): on a pool of one node, two turns
    /// parked and polled; the first queues and the second's first poll finds
    /// no node, so its wait is replaced and `q` counts the first alone. A
    /// handout wakes the first; spent, it gives its node back, and the second
    /// queues at its next poll. The second resolves only after the second
    /// handout: a starved wait is never read as a wake.
    #[test]
    fn a_turn_resolves_only_on_a_landed_wake_when_the_pool_is_empty() {
        let (mut waiters, nodes) = window_on(ONE_NODE);
        let mut first = waiters.park();
        let mut second = waiters.park();
        assert!(!poll_turn(&mut first) && !poll_turn(&mut second), "both parked");
        assert!(nodes.full() >= 1, "VACUOUS: no first poll found the pool empty");
        assert_eq!((waiters.parked(), waiters.turns_out()), (1, 0), "q counts the queued one");
        waiters.hand_out(0);
        assert_eq!((waiters.parked(), waiters.turns_out()), (0, 1), "the head's wake landed");
        assert!(poll_turn(&mut first), "the first resolves on its wake");
        first.spend();
        assert!(!poll_turn(&mut second), "the second queues on the freed node");
        assert_eq!((waiters.parked(), waiters.turns_out()), (1, 0), "and q counts it");
        assert!(!poll_turn(&mut second), "the second resolves on no starved wait");
        waiters.hand_out(0);
        assert!(poll_turn(&mut second), "the second handout wakes it");
        second.spend();
        assert_eq!((waiters.parked(), waiters.turns_out(), nodes.len()), (0, 0, 0));
    }

    /// A park and its poll apart: a turn parked and not polled is on no list,
    /// so a handout does not reach it and `(q, w)` stays (0, 0); its poll
    /// queues it, (1, 0), and the next handout wakes it.
    #[test]
    fn a_handout_reaches_a_parked_turn_only_after_its_poll() {
        let mut waiters = window();
        let mut turn = waiters.park();
        waiters.hand_out(0);
        assert_eq!((waiters.parked(), waiters.turns_out()), (0, 0), "no writer was queued");
        assert!(!poll_turn(&mut turn), "its poll queues it, not woken");
        assert_eq!((waiters.parked(), waiters.turns_out()), (1, 0));
        waiters.hand_out(0);
        assert!(poll_turn(&mut turn), "the next handout wakes it");
        turn.spend();
    }

    /// A turn dropped while its wait is on no list (the arm's second turn,
    /// dropped before a node frees): `q` and `w` do not move, settle's one
    /// poll starves again, and the pool holds no node of it.
    #[test]
    fn a_turn_dropped_with_its_wait_on_no_list_moves_no_count() {
        let (mut waiters, nodes) = window_on(ONE_NODE);
        let mut first = waiters.park();
        let mut second = waiters.park();
        assert!(!poll_turn(&mut first) && !poll_turn(&mut second), "both parked");
        let starved = nodes.full();
        assert!(starved >= 1, "VACUOUS: the second's first poll found a node");
        drop(second);
        assert_eq!((waiters.parked(), waiters.turns_out()), (1, 0), "no count moved");
        assert_eq!(nodes.full(), starved + 1, "settle's one poll starved again");
        assert_eq!(nodes.len(), 1, "the pool holds the first's node alone");
        drop(first);
        assert_eq!((waiters.parked(), nodes.len()), (0, 0));
    }

    /// A spend at `w` = 2: two turns landed, one spent, `w` reads 1, where a
    /// second settle would read 0, which the saturating count would hide at
    /// `w` = 1.
    #[test]
    fn a_spend_settles_one_turn_out() {
        let mut waiters = window();
        let mut turns: Vec<ExtentTurn> = (0..2).map(|_| parked(&mut waiters)).collect();
        waiters.hand_out(0);
        assert_eq!(waiters.turns_out(), 2, "both woken");
        let mut first = turns.remove(0);
        assert!(poll_turn(&mut first));
        first.spend();
        assert_eq!(waiters.turns_out(), 1, "one spend settles one turn out");
        drop(turns);
    }
}
