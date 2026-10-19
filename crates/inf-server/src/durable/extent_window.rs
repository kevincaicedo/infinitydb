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
//! open or closed: every turn has a unit (I12, `len + w ≤ 16`). A fold frees
//! units and wakes no one. There is no retry on the window: a woken writer
//! publishes or leaves, and a turn that leaves gives its unit back, to the
//! queue's head while the window is open and to the next handout while it
//! is closed. So no frame waits for an extent registered after its hold
//! began, beyond the turns already out, and position `q` is woken within
//! `⌈q / 16⌉` turnovers.
//!
//! The counts are exact: `q` writers parked, `w` turns out. A turn's drop
//! polls its own wait once: a wake that had landed was a turn out, any
//! other a parked writer.

use core::cell::Cell;
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, Waker};
use std::rc::Rc;

use inf_log::limits::EXTENT_SEAL_WINDOW_ENTRIES;
use inf_runtime::WaitList;
use inf_runtime::gate::ListWait;

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

/// The arrival test (I12): an arrival reserves only while the window is
/// `open` and no writer is `parked` ahead of it, and, with `turns_out`,
/// only while a unit is free beyond the `len` entries and the turns.
pub(crate) fn admit(open: bool, parked: usize, len: usize, turns_out: usize) -> Arrival {
    if !open || parked > 0 {
        return Arrival::Park;
    }
    if turns_out == 0 || len.saturating_add(turns_out) < ENTRIES {
        Arrival::Reserve
    } else {
        Arrival::Park
    }
}

/// The queue's state, shared with every turn so a turn settles its own
/// count when it drops.
struct Queue {
    open: Cell<bool>,
    /// `q`: writers parked whose wake has not landed.
    parked: Cell<usize>,
    /// `w`: turns out — writers woken and not yet published or gone.
    turns_out: Cell<usize>,
    list: WaitList<()>,
}

impl Queue {
    /// Wakes the queue's head, if a writer is parked: its place becomes a
    /// turn out. Returns whether a wake landed.
    fn wake_head(&self) -> bool {
        if self.parked.get() == 0 {
            return false;
        }
        let landed = self.list.wake_one(());
        debug_assert!(landed, "each parked writer is a live waiter of the list");
        if landed {
            self.parked.set(self.parked.get().saturating_sub(1));
            self.turns_out.set(self.turns_out.get().saturating_add(1));
        }
        landed
    }
}

/// The extent-seal window's queue (ADR-0163 D2): one per cell, in the
/// durable cell beside the staging wait list.
pub(crate) struct ExtentWaiters {
    queue: Rc<Queue>,
    /// Park episodes (cell scope): every writer that joined the queue.
    parks: u64,
    /// Turns that found the window full (cell scope); I12 makes it 0.
    turns_refused: u64,
}

impl ExtentWaiters {
    /// An open window with nobody queued.
    pub(crate) fn new() -> ExtentWaiters {
        let queue = Queue {
            open: Cell::new(true),
            parked: Cell::new(0),
            turns_out: Cell::new(0),
            list: WaitList::new(),
        };
        ExtentWaiters { queue: Rc::new(queue), parks: 0, turns_refused: 0 }
    }

    /// The arrival test for a writer with no turn, against the window's
    /// `len` entries.
    pub(crate) fn arrival(&self, len: usize) -> Arrival {
        let queue = &self.queue;
        admit(queue.open.get(), queue.parked.get(), len, queue.turns_out.get())
    }

    /// Parks a writer at the queue's tail (`q + 1`): one park episode. The
    /// writer awaits the turn, which resolves once a handout wakes it.
    pub(crate) fn park(&mut self) -> ExtentTurn {
        let queue = &self.queue;
        queue.parked.set(queue.parked.get().saturating_add(1));
        self.parks = self.parks.saturating_add(1);
        ExtentTurn { queue: Rc::clone(queue), wait: Some(queue.list.wait(())) }
    }

    /// A turn found the window full, which I12 rules out: debug-asserted,
    /// and counted, so the release form reads the count. The turn gives its
    /// unit back and the writer parks again at the tail.
    pub(crate) fn refuse_turn(&mut self, turn: ExtentTurn) -> ExtentTurn {
        debug_assert!(false, "a turn found the extent-seal window full (len + w ≤ 16 broken)");
        self.turns_refused = self.turns_refused.saturating_add(1);
        drop(turn);
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
        debug_assert!(len.saturating_add(turns_out) <= ENTRIES, "every turn has a unit (I12)");
        let free = ENTRIES.saturating_sub(len).saturating_sub(turns_out);
        let wakes = free.min(queue.parked.get());
        for _ in 0..wakes {
            if !queue.wake_head() {
                break;
            }
        }
    }

    /// Writers parked now (`q`, cell scope).
    pub(crate) fn parked(&self) -> usize {
        self.queue.parked.get()
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
    pub(crate) fn turns_refused(&self) -> u64 {
        self.turns_refused
    }
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
    /// The turn's wait; `None` once the turn is spent on a publish.
    wait: Option<ListWait<()>>,
}

impl ExtentTurn {
    /// The writer published its extent with this turn: the unit is the
    /// window's entry now (`w − 1`), and nothing is passed on.
    pub(crate) fn spend(mut self) {
        let landed = self.wait.as_mut().is_some_and(landed);
        debug_assert!(landed, "a turn is spent only after its wake landed");
        if landed {
            let queue = &self.queue;
            queue.turns_out.set(queue.turns_out.get().saturating_sub(1));
        }
        self.wait = None;
    }
}

/// One poll of a turn's wait: whether its wake has landed. A landed wait is
/// left done, so dropping it passes nothing on.
fn landed(wait: &mut ListWait<()>) -> bool {
    let mut cx = Context::from_waker(Waker::noop());
    Pin::new(wait).poll(&mut cx).is_ready()
}

impl Future for ExtentTurn {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        match Pin::into_inner(self).wait.as_mut() {
            Some(wait) => Pin::new(wait).poll(cx),
            None => Poll::Ready(()),
        }
    }
}

impl Drop for ExtentTurn {
    fn drop(&mut self) {
        let Some(mut wait) = self.wait.take() else { return };
        let queue = &self.queue;
        if landed(&mut wait) {
            queue.turns_out.set(queue.turns_out.get().saturating_sub(1));
            if queue.open.get() {
                queue.wake_head();
            }
        } else {
            queue.parked.set(queue.parked.get().saturating_sub(1));
        }
        // An unlanded wait cancels in place; a landed one was polled done.
        drop(wait);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `admit`'s table (I12): every arrival behind a parked writer or into a
    /// closed window parks; with turns out an arrival reserves only a unit
    /// free beyond theirs; with none, the window answers.
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

    /// A handout wakes `min(16 − len − w, q)` writers from the head and no
    /// more, whatever the number parked; the woken are turns out.
    #[test]
    fn a_handout_wakes_at_most_the_free_units_from_the_head() {
        let mut waiters = ExtentWaiters::new();
        assert!(waiters.close(), "the window starts open");
        let mut turns: Vec<ExtentTurn> = (0..40).map(|_| waiters.park()).collect();
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
        let mut waiters = ExtentWaiters::new();
        let mut turns: Vec<ExtentTurn> = (0..3).map(|_| waiters.park()).collect();
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
        let mut waiters = ExtentWaiters::new();
        let mut turns: Vec<ExtentTurn> = (0..2).map(|_| waiters.park()).collect();
        waiters.hand_out(15);
        let mut first = turns.remove(0);
        assert!(poll_turn(&mut first));
        first.spend();
        assert_eq!((waiters.parked(), waiters.turns_out()), (1, 0));
        assert!(!poll_turn(&mut turns[0]), "the spent unit went to the window, not the queue");
    }
}
