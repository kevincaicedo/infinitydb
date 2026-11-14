//! Leg A's gate: the cold-read host's keyed gate as it stood before its
//! fixed stores, copied whole with the host (`cold.rs`), so every A leg
//! measures the same code after the engine's gate changes, `has_waiter`
//! with it. Its std table is under the bench root's allow.

use core::cell::RefCell;
use core::future::Future;
use core::hash::Hash;
use core::pin::Pin;
use core::task::{Context, Poll, Waker};
use std::collections::HashMap;
use std::rc::Rc;

use inf_foundation::BuildIntHasher;

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
    slots: Rc<RefCell<HashMap<K, SlotState<V>, BuildIntHasher>>>,
}

impl<K: Eq + Hash + Copy, V> Default for KeyedGate<K, V> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: Eq + Hash + Copy, V> KeyedGate<K, V> {
    pub fn new() -> KeyedGate<K, V> {
        KeyedGate { slots: Rc::new(RefCell::new(HashMap::default())) }
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

    /// Whether `key` still has a live, undelivered waiter (M4-S10): the
    /// cold-read drain consults this to skip intents whose command was
    /// cancelled while queued — a dead command must not spend device
    /// bandwidth. `Delivered` counts as consumed (no live *waiter*).
    pub fn has_waiter(&self, key: &K) -> bool {
        matches!(self.slots.borrow().get(key), Some(SlotState::Registered | SlotState::Waiting(_)))
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
    slots: Rc<RefCell<HashMap<K, SlotState<V>, BuildIntHasher>>>,
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
