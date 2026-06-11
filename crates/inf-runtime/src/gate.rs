//! Typed suspension primitives — the only ways a cell future may suspend
//! (ADR-0003). Each is a thin shape over one cell-local pattern: a future
//! registers its waker under a key; the event source completes the key and
//! wakes exactly the right task(s). No atomics, no locks (L1).
//!
//! - [`FabricGate`]: reply-keyed (fabric token → reply payload), M0.
//! - [`IoGate`]: completion-keyed (disk reads M7; exists now so the seam is
//!   frozen).
//! - [`WaitList`]: key-keyed FIFO (M1 blocking ops: BLPOP, WAIT…).
//! - [`WatermarkGate`]: LSN-keyed (M2 fsync-gated acks).
//!
//! Single-waiter gates (`FabricGate`, `IoGate`) key each waiter by a unique
//! token, so "wakes exactly one task" holds by construction. Completion may
//! arrive before the waiter first polls (same-iteration delivery): the value
//! parks in the slot and the first poll returns `Ready` without suspending.
//! All waiter futures are cancellation-safe: dropping one deregisters it.

use core::cell::{Cell, RefCell};
use core::future::Future;
use core::hash::Hash;
use core::pin::Pin;
use core::task::{Context, Poll, Waker};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::rc::Rc;

use crate::driver::CompletionResult;
use crate::token::CompletionToken;

// ---- KeyedGate (FabricGate / IoGate) ----------------------------------------

enum SlotState<V> {
    /// A waiter future exists but has not suspended yet.
    Registered,
    /// The waiter suspended; wake this when the value arrives.
    Waiting(Waker),
    /// Value arrived before the waiter polled (or between polls).
    Delivered(V),
}

/// Single-waiter, value-carrying gate keyed by `K`. The primitive behind
/// [`FabricGate`] and [`IoGate`].
pub struct KeyedGate<K: Eq + Hash + Copy, V> {
    slots: Rc<RefCell<HashMap<K, SlotState<V>>>>,
}

impl<K: Eq + Hash + Copy, V> Default for KeyedGate<K, V> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: Eq + Hash + Copy, V> KeyedGate<K, V> {
    pub fn new() -> KeyedGate<K, V> {
        KeyedGate { slots: Rc::new(RefCell::new(HashMap::new())) }
    }

    /// Register interest in `key` and get the future that resolves when
    /// [`Self::complete`] delivers the value.
    ///
    /// # Panics
    /// Panics if `key` already has a registered waiter — keys are unique
    /// tokens; a duplicate is a routing bug, never load.
    pub fn waiter(&self, key: K) -> GateWait<K, V> {
        let prior = self.slots.borrow_mut().insert(key, SlotState::Registered);
        assert!(prior.is_none(), "duplicate gate waiter for key");
        GateWait { slots: Rc::clone(&self.slots), key, done: false }
    }

    /// Deliver `value` for `key`, waking the waiter if it is suspended.
    /// Delivering before the waiter polls parks the value (same-iteration
    /// replies). Returns `false` (dropping `value`) when no waiter is
    /// registered — a cancelled/stale reply; callers count it, never panic.
    ///
    /// # Panics
    /// Panics on double-delivery for a key — duplicate replies are a
    /// protocol bug upstream.
    pub fn complete(&self, key: K, value: V) -> bool {
        let mut slots = self.slots.borrow_mut();
        match slots.get_mut(&key) {
            None => false,
            Some(slot) => match core::mem::replace(slot, SlotState::Delivered(value)) {
                SlotState::Registered => true,
                SlotState::Waiting(waker) => {
                    // Wake outside the table borrow: the waker only touches
                    // the executor ready queue, but stay conservative.
                    drop(slots);
                    waker.wake();
                    true
                }
                SlotState::Delivered(_) => panic!("gate key completed twice"),
            },
        }
    }

    /// Registered/parked entries (tests + leak asserts).
    pub fn pending(&self) -> usize {
        self.slots.borrow().len()
    }
}

impl<K: Eq + Hash + Copy, V> Clone for KeyedGate<K, V> {
    /// Gates are shared handles (event source + futures share one table).
    fn clone(&self) -> Self {
        KeyedGate { slots: Rc::clone(&self.slots) }
    }
}

impl<K: Eq + Hash + Copy, V> core::fmt::Debug for KeyedGate<K, V> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "KeyedGate {{ pending: {} }}", self.pending())
    }
}

/// Future returned by [`KeyedGate::waiter`]. Dropping it before completion
/// deregisters the key; a late `complete` then returns `false` instead of
/// waking a dead task.
pub struct GateWait<K: Eq + Hash + Copy, V> {
    slots: Rc<RefCell<HashMap<K, SlotState<V>>>>,
    key: K,
    done: bool,
}

// No self-references — state lives behind the shared `Rc` table.
impl<K: Eq + Hash + Copy, V> Unpin for GateWait<K, V> {}

impl<K: Eq + Hash + Copy, V> Future for GateWait<K, V> {
    type Output = V;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<V> {
        let this = Pin::into_inner(self);
        let mut slots = this.slots.borrow_mut();
        match slots.get_mut(&this.key) {
            Some(SlotState::Delivered(_)) => {
                let Some(SlotState::Delivered(value)) = slots.remove(&this.key) else {
                    unreachable!("matched Delivered above")
                };
                this.done = true;
                Poll::Ready(value)
            }
            Some(slot) => {
                *slot = SlotState::Waiting(cx.waker().clone());
                Poll::Pending
            }
            None => panic!("GateWait polled after completion"),
        }
    }
}

impl<K: Eq + Hash + Copy, V> Drop for GateWait<K, V> {
    fn drop(&mut self) {
        if !self.done {
            self.slots.borrow_mut().remove(&self.key);
        }
    }
}

/// Reply-keyed gate for fabric round trips (M0-S10 routes `FabricToken`
/// values here as raw `u64` — `inf-fabric` sits above this crate in the
/// dependency DAG, so the payload and token types are the caller's).
pub type FabricGate<V> = KeyedGate<u64, V>;

/// Completion-keyed gate for suspending on backend I/O (M7 disk reads; the
/// seam exists from M0 so the executor contract is complete).
pub type IoGate = KeyedGate<CompletionToken, CompletionResult>;

// ---- WaitList ---------------------------------------------------------------

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum WaiterState {
    Queued,
    Woken,
    Cancelled,
}

struct Waiter {
    state: Cell<WaiterState>,
    waker: RefCell<Option<Waker>>,
}

type WaitQueues<K> = Rc<RefCell<HashMap<K, VecDeque<Rc<Waiter>>>>>;

/// Key-keyed FIFO wait list for blocking ops (M1+: BLPOP, XREAD BLOCK…).
/// Multiple tasks may wait on one key; `wake_one` hands the key to the
/// longest-waiting live task, `wake_all` to everyone.
pub struct WaitList<K: Eq + Hash + Copy> {
    queues: WaitQueues<K>,
}

impl<K: Eq + Hash + Copy> Default for WaitList<K> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: Eq + Hash + Copy> WaitList<K> {
    pub fn new() -> WaitList<K> {
        WaitList { queues: Rc::new(RefCell::new(HashMap::new())) }
    }

    /// Join the FIFO for `key`. The future resolves when a mutation wakes
    /// this waiter; dropping it (timeout, disconnect) cancels in place and
    /// — if the wake already landed — passes the baton to the next waiter,
    /// so a wake is never silently lost.
    pub fn wait(&self, key: K) -> ListWait<K> {
        let waiter =
            Rc::new(Waiter { state: Cell::new(WaiterState::Queued), waker: RefCell::new(None) });
        self.queues.borrow_mut().entry(key).or_default().push_back(Rc::clone(&waiter));
        ListWait { queues: Rc::clone(&self.queues), key, waiter, done: false }
    }

    /// Wake the longest-waiting live task on `key`. Returns whether one was
    /// woken.
    pub fn wake_one(&self, key: K) -> bool {
        wake_one_in(&self.queues, key)
    }

    /// Wake every task waiting on `key`. Returns how many.
    pub fn wake_all(&self, key: K) -> usize {
        let Some(queue) = self.queues.borrow_mut().remove(&key) else { return 0 };
        let mut woken = 0;
        for waiter in queue {
            if waiter.state.get() == WaiterState::Queued {
                waiter.state.set(WaiterState::Woken);
                if let Some(w) = waiter.waker.borrow_mut().take() {
                    w.wake();
                }
                woken += 1;
            }
        }
        woken
    }

    /// Live waiters across all keys (tests + leak asserts). Cancelled
    /// entries pending lazy cleanup are not counted.
    pub fn waiting(&self) -> usize {
        self.queues
            .borrow()
            .values()
            .flat_map(|q| q.iter())
            .filter(|w| w.state.get() == WaiterState::Queued)
            .count()
    }
}

impl<K: Eq + Hash + Copy> Clone for WaitList<K> {
    /// Shared handle: mutation sites and waiting futures share the queues.
    fn clone(&self) -> Self {
        WaitList { queues: Rc::clone(&self.queues) }
    }
}

impl<K: Eq + Hash + Copy> core::fmt::Debug for WaitList<K> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "WaitList {{ waiting: {} }}", self.waiting())
    }
}

fn wake_one_in<K: Eq + Hash + Copy>(queues: &WaitQueues<K>, key: K) -> bool {
    let mut map = queues.borrow_mut();
    let Some(queue) = map.get_mut(&key) else { return false };
    // Skip lazily-cancelled entries.
    let woken = loop {
        match queue.pop_front() {
            None => break None,
            Some(w) if w.state.get() == WaiterState::Cancelled => continue,
            Some(w) => break Some(w),
        }
    };
    if queue.is_empty() {
        map.remove(&key);
    }
    drop(map);
    match woken {
        None => false,
        Some(waiter) => {
            waiter.state.set(WaiterState::Woken);
            if let Some(w) = waiter.waker.borrow_mut().take() {
                w.wake();
            }
            true
        }
    }
}

/// Future returned by [`WaitList::wait`].
pub struct ListWait<K: Eq + Hash + Copy> {
    queues: WaitQueues<K>,
    key: K,
    waiter: Rc<Waiter>,
    done: bool,
}

// No self-references — state lives behind the shared `Rc` queues.
impl<K: Eq + Hash + Copy> Unpin for ListWait<K> {}

impl<K: Eq + Hash + Copy> Future for ListWait<K> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = Pin::into_inner(self);
        match this.waiter.state.get() {
            WaiterState::Woken => {
                this.done = true;
                Poll::Ready(())
            }
            WaiterState::Queued => {
                *this.waiter.waker.borrow_mut() = Some(cx.waker().clone());
                Poll::Pending
            }
            WaiterState::Cancelled => panic!("ListWait polled after cancellation"),
        }
    }
}

impl<K: Eq + Hash + Copy> Drop for ListWait<K> {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        match self.waiter.state.get() {
            // Still queued: cancel in place; `wake_one` skips us lazily.
            WaiterState::Queued => self.waiter.state.set(WaiterState::Cancelled),
            // Woken but never observed (e.g. timed out in the same
            // iteration): pass the baton so the wake is not lost.
            WaiterState::Woken => {
                wake_one_in(&self.queues, self.key);
            }
            WaiterState::Cancelled => {}
        }
    }
}

// ---- WatermarkGate ----------------------------------------------------------

struct WatermarkInner {
    watermark: Cell<u64>,
    waiters: RefCell<BTreeMap<u64, Vec<Rc<Waiter>>>>,
}

/// LSN-keyed gate: tasks wait for the durability watermark to reach their
/// LSN; `advance` wakes every task at or below the new watermark (M2 group
/// commit acks). A waiter at or below the current watermark is ready on its
/// first poll.
#[derive(Clone)]
pub struct WatermarkGate {
    inner: Rc<WatermarkInner>,
}

impl Default for WatermarkGate {
    fn default() -> Self {
        Self::new()
    }
}

impl WatermarkGate {
    pub fn new() -> WatermarkGate {
        WatermarkGate {
            inner: Rc::new(WatermarkInner {
                watermark: Cell::new(0),
                waiters: RefCell::new(BTreeMap::new()),
            }),
        }
    }

    pub fn watermark(&self) -> u64 {
        self.inner.watermark.get()
    }

    /// Wait until the watermark reaches `lsn`.
    pub fn waiter(&self, lsn: u64) -> WatermarkWait {
        let waiter = Rc::new(Waiter {
            state: Cell::new(if self.inner.watermark.get() >= lsn {
                WaiterState::Woken
            } else {
                WaiterState::Queued
            }),
            waker: RefCell::new(None),
        });
        if waiter.state.get() == WaiterState::Queued {
            self.inner.waiters.borrow_mut().entry(lsn).or_default().push(Rc::clone(&waiter));
        }
        WatermarkWait { waiter, done: false }
    }

    /// Advance the watermark (monotonic; lower values are no-ops) and wake
    /// every waiter at or below it. Returns how many tasks were woken.
    pub fn advance(&self, to: u64) -> usize {
        if to <= self.inner.watermark.get() {
            return 0;
        }
        self.inner.watermark.set(to);
        let due = {
            let mut waiters = self.inner.waiters.borrow_mut();
            let still_waiting = waiters.split_off(&(to + 1));
            core::mem::replace(&mut *waiters, still_waiting)
        };
        let mut woken = 0;
        for waiter in due.into_values().flatten() {
            if waiter.state.get() == WaiterState::Queued {
                waiter.state.set(WaiterState::Woken);
                if let Some(w) = waiter.waker.borrow_mut().take() {
                    w.wake();
                }
                woken += 1;
            }
        }
        woken
    }

    /// Live waiters (tests + leak asserts).
    pub fn waiting(&self) -> usize {
        self.inner
            .waiters
            .borrow()
            .values()
            .flat_map(|v| v.iter())
            .filter(|w| w.state.get() == WaiterState::Queued)
            .count()
    }
}

impl core::fmt::Debug for WatermarkGate {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "WatermarkGate {{ watermark: {}, waiting: {} }}",
            self.watermark(),
            self.waiting()
        )
    }
}

/// Future returned by [`WatermarkGate::waiter`].
pub struct WatermarkWait {
    waiter: Rc<Waiter>,
    done: bool,
}

impl Future for WatermarkWait {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = Pin::into_inner(self);
        match this.waiter.state.get() {
            WaiterState::Woken => {
                this.done = true;
                Poll::Ready(())
            }
            WaiterState::Queued => {
                *this.waiter.waker.borrow_mut() = Some(cx.waker().clone());
                Poll::Pending
            }
            WaiterState::Cancelled => unreachable!("watermark waiters are never cancelled"),
        }
    }
}

impl Drop for WatermarkWait {
    fn drop(&mut self) {
        // Lazy cleanup: mark cancelled; `advance` skips dead entries. (The
        // BTreeMap entry itself is removed when its LSN is crossed.)
        if !self.done && self.waiter.state.get() == WaiterState::Queued {
            self.waiter.state.set(WaiterState::Cancelled);
        }
    }
}
