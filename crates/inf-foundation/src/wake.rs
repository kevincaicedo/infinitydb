//! The wake bridge (ADR-0003 A1.2): the part of a cell executor's wake header that any thread may
//! touch, and the executor's remote stack, on which another thread queues a wake for the cell to
//! deliver.
//!
//! A standard `Waker` is `Send + Sync`, so each of its operations must be thread-safe. A cell's
//! executor answers them on two lanes. On the thread that built the executor, which its
//! [`ThreadMark`] names, a waker operation takes the owner lane: cell-local state, the executor's,
//! and no atomic read-modify-write. On any other thread, and on every thread once the executor is
//! gone, it takes the foreign lane: this module, whose types are `Sync` by their fields, all
//! atomics, with no `unsafe`.
//!
//! **The lane word** ([`WakeBridge`], one `AtomicU64`) holds the foreign count `c`, the wakers the
//! foreign lane counted (61 bits, two's complement: a lane falls below 0 by dropping wakers the
//! other lane counted), and three flags: `Q`, the bridge is on the remote stack; `W`, a wake is
//! pending; `X`, the header is closed (its executor is gone). The header's live wakers are
//! `w = o + c`, `o` the owner lane's count, which the executor keeps and [`WakeBridge::close`]
//! folds into `c`. Each lane's count is compared with [`LANE_COUNT_MAX`] before it grows.
//!
//! **A foreign call** ([`WakeBridge::foreign_clone`], [`WakeBridge::foreign_wake_by_ref`],
//! [`WakeBridge::foreign_wake`], [`WakeBridge::foreign_drop`]) first adds 1 to the stack's
//! `waker_foreign_ops`, while the waker it holds keeps the header and the stack allocated; then
//! makes one compare-and-swap of the lane word, retried only after another thread's swap
//! succeeded (lock-free); then, if its swap set `Q` from clear, pushes the bridge. The swap, or
//! the push, is the call's last access to the header and the stack, unless it made the caller
//! the header's freer ([`Released::Last`]).
//!
//! **The drain** is the owner's: [`RemoteStack::take`] takes the whole stack in one swap, and
//! for each bridge [`Drained::deliver_next`] reads its link first, then clears `Q` and `W` in one
//! read-modify-write. The executor's drop closes each header it holds ([`WakeBridge::close`]:
//! the mark set to [`MARK_CLOSED`], then `o` folded into `c` with `X` in one swap) and ends with
//! [`RemoteStack::take_last`], which leaves the head closed, so a push decided before a close
//! either lands before the last take or finds the head closed and clears its own `Q`.
//!
//! **Frees.** While the executor lives only its thread frees a header, on `w = 0` with `Q` clear
//! ([`WakeBridge::quiet`]). A closed header is freed by the thread whose read-modify-write leaves
//! it at `c = 0` with `Q` clear ([`Closing::Free`], [`Released::Last`], [`Settled::Last`]); that
//! thread then releases the stack's count of it, and the release that takes the count to zero
//! frees the stack ([`Freed::Stack`]), `Arc`'s protocol.
//!
//! **Orderings.** Each load-bearing ordering is a named const, which the Loom models check
//! (`RUSTFLAGS="--cfg loom"`). Every other access is `Relaxed`, with its reason at its use.

use core::cell::Cell;
use core::ptr::{self, NonNull};

use crate::limits::LANE_COUNT_MAX;

/// Atomics swapped for Loom's model-checked versions under `--cfg loom` (the pattern of the
/// `issue` module; ADR-0159 A1.7). The thread-mark counter is not among them ([`ThreadMark`]).
#[cfg(loom)]
mod sync {
    pub(super) use loom::sync::atomic::{AtomicPtr, AtomicU64, Ordering};
}

#[cfg(not(loom))]
mod sync {
    pub(super) use core::sync::atomic::{AtomicPtr, AtomicU64, Ordering};
}

use sync::{AtomicPtr, AtomicU64, Ordering};

// ---- Thread marks --------------------------------------------------------------------------

/// The owner mark of a closed header: no thread's, since the mark counter never issues it.
pub const MARK_CLOSED: u64 = u64::MAX;

/// The next mark to claim. A `core` atomic under Loom too: a `static` cannot hold Loom's atomics,
/// which are not `const`-constructible, and a mark's uniqueness needs the read-modify-write
/// alone, no ordering (ADR-0003 A1.2.8). Width: marks 1 to `MARK_CLOSED − 1` are issued, one per
/// executor-building thread of the process (INF 584 years at 10⁹ claims a second); past them a
/// claim aborts the process, so no mark is issued twice, and 0 and `MARK_CLOSED` never.
static NEXT_MARK: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(1);

std::thread_local! {
    /// This thread's mark: 0 until [`ThreadMark::claim`]. `const` and with no `Drop`, so it is
    /// readable during thread-local destruction: a waker another thread local's destructor drops
    /// reads it as any other does.
    static THREAD_MARK: Cell<u64> = const { Cell::new(0) };
}

/// A thread's mark, claimed by the thread that builds an executor: process-unique, never 0 and
/// never [`MARK_CLOSED`]. A bridge carries its executor's, so comparing it with
/// [`ThreadMark::current`] tells the owner lane from the foreign one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ThreadMark(u64);

impl ThreadMark {
    /// The calling thread's mark word: 0 on a thread that has claimed none, so it matches no
    /// bridge.
    #[inline(always)]
    #[must_use]
    pub fn current() -> u64 {
        THREAD_MARK.with(Cell::get)
    }

    /// The calling thread's mark, claimed from the process-wide counter on its first call. Two
    /// executors on one thread share it, so each takes the owner lane on that thread.
    #[must_use]
    pub fn claim() -> ThreadMark {
        let held = ThreadMark::current();
        if held != 0 {
            return ThreadMark(held);
        }
        let issued = NEXT_MARK.fetch_update(
            core::sync::atomic::Ordering::Relaxed,
            core::sync::atomic::Ordering::Relaxed,
            |next| (next < MARK_CLOSED).then_some(next + 1),
        );
        // The counter stops at `MARK_CLOSED`: every claim past the last mark aborts.
        let Ok(mark) = issued else { std::process::abort() };
        THREAD_MARK.with(|slot| slot.set(mark));
        ThreadMark(mark)
    }

    /// The mark's value.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

// ---- The lane word -------------------------------------------------------------------------

/// `Q`: the bridge is on the remote stack (one entry per header, by this flag).
const QUEUED: u64 = 1;
/// `W`: a wake is pending, for the drain to deliver.
const WOKEN: u64 = 1 << 1;
/// `X`: the header is closed; its executor is gone.
const CLOSED: u64 = 1 << 2;
const FLAGS: u64 = QUEUED | WOKEN | CLOSED;
/// The foreign count's place: bits 3 to 63.
const COUNT_SHIFT: u32 = 3;

// Either lane's count, the floor −LANE_COUNT_MAX included, fits the word's 61-bit field.
const _: () = assert!(LANE_COUNT_MAX <= i64::MAX >> COUNT_SHIFT, "the count fits 61 bits");

/// One value of a lane word.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Lane(u64);

impl Lane {
    fn count(self) -> i64 {
        self.0.cast_signed() >> COUNT_SHIFT
    }

    fn has(self, flag: u64) -> bool {
        self.0 & flag != 0
    }

    /// The word with `count` in its count field and its flags kept. A count past the field's
    /// range wraps it: the bound's compare, before every increment, is what keeps it in range.
    fn with_count(self, count: i64) -> Lane {
        Lane((count.cast_unsigned() << COUNT_SHIFT) | (self.0 & FLAGS))
    }

    fn with(self, flags: u64) -> Lane {
        Lane(self.0 | flags)
    }
}

// ---- Orderings: each load-bearing one a named const -----------------------------------------

/// The foreign lane's swap of the lane word (ADR-0003 A1.7). Its release orders a call's last
/// access, when the call pushes nothing (`Q` already set, or the header closed), before the free
/// a later swap decides; its acquire makes a freeing swap follow every earlier one.
const LANE_ORDERING: Ordering = Ordering::AcqRel;

/// The push's compare-and-swap of the stack's head: its release publishes the bridge's link
/// to the take that acquires it.
const PUSH_ORDERING: Ordering = Ordering::Release;

/// The drain's swap of the stack's head: its acquire makes every link it then reads the one its
/// push wrote.
const TAKE_ORDERING: Ordering = Ordering::Acquire;

/// The drain's clear of `Q` and `W`. Its acquire orders the foreign accesses before it (the swap
/// of a last drop that found `Q` set and pushed nothing) before the owner's free or reuse; its
/// release orders the drain's read of the link before the next push's write of it.
const CLEAR_ORDERING: Ordering = Ordering::AcqRel;

/// `close`'s swap: its release orders the owner part's drop in place before the free another
/// thread's swap decides.
const CLOSE_ORDERING: Ordering = Ordering::AcqRel;

/// The release of a stack count, `Arc`'s protocol: a `Release` decrement, so every access the
/// releasing thread made to the stack happens before the free; and, where the count reaches
/// zero, an `Acquire` load, so the freer follows every other release.
const RETAIN_ORDERING: Ordering = Ordering::Release;
/// The acquire half of the release rule above, at zero.
const RETAIN_ZERO_ORDERING: Ordering = Ordering::Acquire;

// ---- The tomb: a Loom model's view of a free ------------------------------------------------

#[cfg(not(all(test, loom)))]
mod tomb {
    /// Zero-sized outside the Loom models: a visit compiles to nothing.
    #[derive(Debug)]
    pub(super) struct Tomb;

    impl Tomb {
        pub(super) fn new() -> Tomb {
            Tomb
        }

        #[inline(always)]
        pub(super) fn visit(&self) {}
    }
}

#[cfg(all(test, loom))]
#[allow(clippy::disallowed_types, reason = "test-only: the Loom models' tomb is shared by a Mutex")]
mod tomb {
    use std::sync::{Mutex, PoisonError};

    /// In the Loom models, a cell that a free writes ([`Tomb::bury`]) and that each access to
    /// what it frees reads first ([`Tomb::visit`]): a free not ordered after an access, or an
    /// access after a free, is a causality violation Loom reports. The `Mutex` makes the cell
    /// `Sync` with no `unsafe`; it never contends, since Loom runs a model's threads as coroutines
    /// of one OS thread and a cell access does not yield, and Loom does not model it, so it adds
    /// no happens-before edge.
    #[derive(Debug)]
    pub(super) struct Tomb(Mutex<loom::cell::UnsafeCell<()>>);

    impl Tomb {
        pub(super) fn new() -> Tomb {
            Tomb(Mutex::new(loom::cell::UnsafeCell::new(())))
        }

        pub(super) fn visit(&self) {
            self.0.lock().unwrap_or_else(PoisonError::into_inner).with(|_| ());
        }

        pub(super) fn bury(&self) {
            self.0.lock().unwrap_or_else(PoisonError::into_inner).with_mut(|_| ());
        }
    }
}

use tomb::Tomb;

// ---- The bridge and the stack --------------------------------------------------------------

/// What another thread may touch of a wake header: atomics only, so `Sync` by its fields alone.
#[derive(Debug)]
pub struct WakeBridge {
    /// The foreign count and the flags `Q`, `W`, `X`.
    lane: AtomicU64,
    /// The next bridge on the remote stack while `Q` is set.
    link: AtomicPtr<WakeBridge>,
    /// The remote stack this header queues on, written once, before the header is first lent.
    stack: AtomicPtr<RemoteStack>,
    /// The executor's thread mark, then [`MARK_CLOSED`] from `close` on.
    mark: AtomicU64,
    tomb: Tomb,
}

/// The executor's remote stack: an intrusive lock-free stack of bridges, pushed by any thread and
/// taken whole by the owner, with the cell's `waker_foreign_ops` and the count of what keeps the
/// stack allocated. Atomics only, so `Sync` by its fields alone.
#[derive(Debug)]
pub struct RemoteStack {
    /// The top bridge, null when empty, [`CLOSED_HEAD`] after the last take.
    head: AtomicPtr<WakeBridge>,
    /// Foreign-lane operations on this cell's headers. A statistic: it wraps at `u64::MAX`, and a
    /// product path makes none while the executor lives.
    foreign_ops: AtomicU64,
    /// The executor's own count, held to its last take, and one per closed header the stack still
    /// keeps; never above the headers alive plus one, which memory bounds.
    counted: AtomicU64,
    tomb: Tomb,
}

/// The head the last take leaves: an address no bridge has (a bridge is aligned to 8).
const CLOSED_HEAD: *mut WakeBridge = ptr::without_provenance_mut(1);

const _: () = assert!(align_of::<WakeBridge>() > 1, "no bridge lives at the closed head's address");

// What another thread reaches is `Sync` by its fields (ADR-0003 A1.2.1).
const _: () = {
    const fn shared<T: Send + Sync>() {}
    shared::<WakeBridge>();
    shared::<RemoteStack>();
};

// The bridge's lane word, link, stack address and mark; the stack's head, count of foreign
// operations and count of what keeps it. Loom's atomics are larger.
#[cfg(not(loom))]
const _: () = assert!(size_of::<WakeBridge>() == 32, "a bridge is four words");
#[cfg(not(loom))]
const _: () = assert!(size_of::<RemoteStack>() == 24, "a remote stack is three words");

/// What a foreign `wake_by_ref` did.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Pushed {
    /// This call set `Q` and pushed the bridge: the next drain delivers the wake.
    Now,
    /// `Q` was set: the entry already on the stack delivers it.
    Already,
    /// The header is closed, so the wake has no effect.
    Closed,
}

/// What a foreign `wake` or `drop` left for its caller.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[must_use]
pub enum Released {
    /// This call set `Q` and pushed the bridge: the owner's drain settles the header.
    Queued,
    /// Nothing: another waker, the entry already on the stack, or the last take settles it.
    Kept,
    /// The header is closed and this call left it at `c = 0` with `Q` clear: the caller frees the
    /// header, then releases the stack's count of it.
    Last,
}

/// What a drain's clear of a closed header's bridge left for the owner.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[must_use]
pub enum Settled {
    /// A waker still names the header: its last drop frees it.
    Kept,
    /// `c = 0` and `Q` now clear: the caller frees the header, then releases the stack's count.
    Last,
}

/// What a live drain's clear found pending.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Delivery {
    /// A foreign wake: the owner runs the owner-lane wake of the header's task.
    Wake,
    /// A foreign count change only (a drop queued the header).
    NoWake,
}

/// What `close` left.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[must_use]
pub enum Closing {
    /// `w = 0` and `Q` clear: the owner frees the header now, then releases its stack count.
    Free,
    /// A waker names it, or a push holds it: the swap that leaves it at `c = 0` with `Q` clear
    /// frees it, on whichever thread makes it.
    Retain,
}

/// What a release of a stack count left.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[must_use]
pub enum Freed {
    /// Another count keeps the stack.
    No,
    /// The count reached zero: the caller frees the stack.
    Stack,
}

/// Where a push went.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Landed {
    Yes,
    /// The last take closed the head: the pusher clears its own `Q`.
    Closed,
}

impl WakeBridge {
    /// A live header's bridge: no waker counted, no flag, `owner`'s mark and `stack`'s address.
    #[must_use]
    pub fn new(stack: &RemoteStack, owner: ThreadMark) -> WakeBridge {
        WakeBridge {
            lane: AtomicU64::new(0),
            link: AtomicPtr::new(ptr::null_mut()),
            stack: AtomicPtr::new(ptr::from_ref(stack).cast_mut()),
            mark: AtomicU64::new(owner.get()),
            tomb: Tomb::new(),
        }
    }

    /// The owner mark: the executor's thread mark, or [`MARK_CLOSED`]. `Relaxed`: only the
    /// owner thread's compare can match it, and that thread wrote it.
    #[inline(always)]
    #[must_use]
    pub fn owner_mark(&self) -> u64 {
        self.mark.load(Ordering::Relaxed)
    }

    /// The remote stack this header queues on. `Relaxed`: written once, before the header is
    /// first lent, which orders it before every waker's use.
    #[must_use]
    pub fn stack(&self) -> *const RemoteStack {
        self.stack.load(Ordering::Relaxed).cast_const()
    }

    /// A foreign clone: `c + 1`, under the bound's compare. `stack` is the bridge's own.
    pub fn foreign_clone(&self, stack: &RemoteStack) {
        debug_assert!(ptr::eq(stack, self.stack()), "a bridge's calls name its own stack");
        stack.count_foreign();
        self.swap_lane(LANE_ORDERING, |lane| {
            let count = lane.count();
            if count >= LANE_COUNT_MAX {
                std::process::abort();
            }
            lane.with_count(count + 1)
        });
    }

    /// A foreign `wake_by_ref`: `W` and `Q` set unless the header is closed, and the bridge
    /// pushed if `Q` was clear. `stack` is the bridge's own.
    pub fn foreign_wake_by_ref(&self, stack: &RemoteStack) -> Pushed {
        debug_assert!(ptr::eq(stack, self.stack()), "a bridge's calls name its own stack");
        stack.count_foreign();
        self.tomb.visit();
        let mut word = self.lane.load(Ordering::Relaxed);
        let old = loop {
            let lane = Lane(word);
            if lane.has(CLOSED) {
                return Pushed::Closed;
            }
            self.tomb.visit();
            match self.lane.compare_exchange(
                word,
                lane.with(WOKEN | QUEUED).0,
                LANE_ORDERING,
                Ordering::Relaxed,
            ) {
                Ok(old) => break Lane(old),
                Err(now) => word = now,
            }
        };
        if old.has(QUEUED) {
            Pushed::Already
        } else {
            match self.queue(stack) {
                Landed::Yes => Pushed::Now,
                Landed::Closed => {
                    let settled = self.unqueue();
                    // The caller's waker keeps `c ≥ 1`, so the clear frees nothing.
                    debug_assert_eq!(settled, Settled::Kept, "a waker still names the header");
                    Pushed::Closed
                }
            }
        }
    }

    /// A foreign `wake`, which consumes its waker: `c − 1` with `W` and `Q` set unless the header
    /// is closed. `stack` is the bridge's own.
    pub fn foreign_wake(&self, stack: &RemoteStack) -> Released {
        self.release_waker(stack, WOKEN | QUEUED)
    }

    /// A foreign drop: `c − 1` with `Q` set unless the header is closed. `stack` is the bridge's
    /// own.
    pub fn foreign_drop(&self, stack: &RemoteStack) -> Released {
        self.release_waker(stack, QUEUED)
    }

    /// Whether no waker names the header and it is not queued: `w = 0` with `Q` clear, the
    /// owner's test before it reuses or frees a header of a live executor. One `Relaxed` load:
    /// a stale value holds every swap ordered before the load, and while a waker lives it reads
    /// `w ≥ 1`, after a foreign last drop `Q` until the owner's own clear; so what it frees on is
    /// that clear or a swap a waker's move ordered before it, and the acquire-release swaps order
    /// every earlier foreign access before the free (ADR-0003 A1.7).
    #[must_use]
    pub fn quiet(&self, owner_count: i64) -> bool {
        let lane = Lane(self.lane.load(Ordering::Relaxed));
        debug_assert!(!lane.has(CLOSED), "a closed header has no owner lane");
        lane.count() + owner_count == 0 && !lane.has(QUEUED)
    }

    /// Closes the header at its executor's drop, after the owner part was dropped in place: the
    /// mark set to [`MARK_CLOSED`], then one swap folds `owner_count` into `c` and sets `X`, under
    /// the bound's compare. From that swap on every operation, on every thread, is foreign.
    pub fn close(&self, owner_count: i64) -> Closing {
        // Relaxed: the owner thread's own compares read it in program order, and no other
        // thread's mark ever matched the one it replaces.
        self.mark.store(MARK_CLOSED, Ordering::Relaxed);
        let old = self.swap_lane(CLOSE_ORDERING, |lane| {
            debug_assert!(!lane.has(CLOSED), "a header is closed once");
            // Each lane's count is within ±LANE_COUNT_MAX, so the sum fits an `i64`.
            let folded = lane.count() + owner_count;
            if folded > LANE_COUNT_MAX {
                std::process::abort();
            }
            lane.with_count(folded).with(CLOSED)
        });
        if old.count() + owner_count == 0 && !old.has(QUEUED) {
            Closing::Free
        } else {
            Closing::Retain
        }
    }

    /// `c − 1` with `raise` set unless closed, then the push or the free that follows.
    fn release_waker(&self, stack: &RemoteStack, raise: u64) -> Released {
        debug_assert!(ptr::eq(stack, self.stack()), "a bridge's calls name its own stack");
        stack.count_foreign();
        let before = self.swap_lane(LANE_ORDERING, |lane| {
            raise_unless_closed(lane.with_count(lane.count() - 1), raise)
        });
        let count = before.count() - 1;
        if before.has(CLOSED) {
            if count == 0 && !before.has(QUEUED) { Released::Last } else { Released::Kept }
        } else if before.has(QUEUED) {
            Released::Kept
        } else {
            match self.queue(stack) {
                Landed::Yes => Released::Queued,
                Landed::Closed => match self.unqueue() {
                    Settled::Kept => Released::Kept,
                    Settled::Last => Released::Last,
                },
            }
        }
    }

    /// One compare-and-swap of the lane word to `next(old)`, retried with the word it read when
    /// another thread's swap succeeded first; returns the word it replaced.
    fn swap_lane(&self, ordering: Ordering, next: impl Fn(Lane) -> Lane) -> Lane {
        self.tomb.visit();
        let mut word = self.lane.load(Ordering::Relaxed);
        loop {
            let new = next(Lane(word));
            self.tomb.visit();
            match self.lane.compare_exchange(word, new.0, ordering, Ordering::Relaxed) {
                Ok(old) => return Lane(old),
                Err(now) => word = now,
            }
        }
    }

    /// The push that follows a swap which set `Q` from clear.
    fn queue(&self, stack: &RemoteStack) -> Landed {
        stack.push(self)
    }

    /// Clears `Q` and `W` in one read-modify-write and says whether that left a closed header
    /// free: the clear of a push that found the head closed, and of the last take. A word with no
    /// `X` decides nothing here: the `close` that follows it in the word's order sees `Q` clear
    /// and decides itself.
    fn unqueue(&self) -> Settled {
        let old = self.clear();
        if old.has(CLOSED) && old.count() == 0 { Settled::Last } else { Settled::Kept }
    }

    fn clear(&self) -> Lane {
        self.tomb.visit();
        Lane(self.lane.fetch_and(!(QUEUED | WOKEN), CLEAR_ORDERING))
    }
}

/// `lane` with `raise` set, unless the header is closed.
fn raise_unless_closed(lane: Lane, raise: u64) -> Lane {
    if lane.has(CLOSED) { lane } else { lane.with(raise) }
}

impl RemoteStack {
    /// An empty stack, counted once: the executor's own count, which it releases after its last
    /// take.
    #[must_use]
    pub fn new() -> RemoteStack {
        RemoteStack {
            head: AtomicPtr::new(ptr::null_mut()),
            foreign_ops: AtomicU64::new(0),
            counted: AtomicU64::new(1),
            tomb: Tomb::new(),
        }
    }

    /// Whether a foreign call queued a bridge: `has_ready`'s check, and `run_ready`'s before its
    /// take, so an idle cell pays one load, not a swap. `Relaxed`: the take acquires.
    #[must_use]
    pub fn pending(&self) -> bool {
        !self.head.load(Ordering::Relaxed).is_null()
    }

    /// Takes every queued bridge in one swap.
    pub fn take(&self) -> Drained {
        let first = self.head.swap(ptr::null_mut(), TAKE_ORDERING);
        debug_assert_ne!(first, CLOSED_HEAD, "no take follows the last");
        Drained { next: first }
    }

    /// The executor's last take: every queued bridge, leaving the head closed, so a later push
    /// lands nowhere and its caller clears its own `Q`.
    pub fn take_last(&self) -> Drained {
        let first = self.head.swap(CLOSED_HEAD, TAKE_ORDERING);
        debug_assert_ne!(first, CLOSED_HEAD, "the last take happens once");
        Drained { next: first }
    }

    /// The cell's `waker_foreign_ops`. `Relaxed`: a statistic.
    #[must_use]
    pub fn foreign_ops(&self) -> u64 {
        self.foreign_ops.load(Ordering::Relaxed)
    }

    /// Counts a header the stack keeps, before its `close`. `Relaxed`, as `Arc::clone`: the
    /// owner holds a count already, so the stack is alive across it.
    pub fn retain(&self) {
        self.counted.fetch_add(1, Ordering::Relaxed);
    }

    /// Releases one count: a freed closed header's, or the executor's own after its last take.
    pub fn release(&self) -> Freed {
        self.tomb.visit();
        if self.counted.fetch_sub(1, RETAIN_ORDERING) != 1 {
            return Freed::No;
        }
        // The acquire half: the freer follows every other release.
        let _ = self.counted.load(RETAIN_ZERO_ORDERING);
        Freed::Stack
    }

    /// Adds one to `waker_foreign_ops`, first in each foreign call: the call's waker then still
    /// keeps the header, and so the stack, allocated (ADR-0003 A1.2.5). `Relaxed`: that place
    /// orders it (ADR-0003 A1.7).
    fn count_foreign(&self) {
        self.tomb.visit();
        self.foreign_ops.fetch_add(1, Ordering::Relaxed);
    }

    /// Pushes `bridge`, whose swap set `Q` from clear. Retried only when another thread's push or
    /// take changed the head first (lock-free).
    fn push(&self, bridge: &WakeBridge) -> Landed {
        let node = ptr::from_ref(bridge).cast_mut();
        self.tomb.visit();
        let mut head = self.head.load(Ordering::Relaxed);
        loop {
            if head == CLOSED_HEAD {
                return Landed::Closed;
            }
            bridge.tomb.visit();
            // Relaxed: the head's release publishes it.
            bridge.link.store(head, Ordering::Relaxed);
            self.tomb.visit();
            match self.head.compare_exchange(head, node, PUSH_ORDERING, Ordering::Relaxed) {
                Ok(_) => return Landed::Yes,
                Err(now) => head = now,
            }
        }
    }
}

impl Default for RemoteStack {
    fn default() -> RemoteStack {
        RemoteStack::new()
    }
}

/// The bridges one take removed from the stack, settled one at a time by the owner. A bridge
/// stays queued (`Q` set) until its turn, so each must be settled; dropping this with entries
/// left would lose their wakes.
#[must_use]
#[derive(Debug)]
pub struct Drained {
    next: *mut WakeBridge,
}

impl Drained {
    /// Settles the next bridge of a live executor's take: reads its link, then clears its `Q` and
    /// `W`, and says whether a wake was pending. `resolve` turns the bridge's address into the
    /// bridge, which its `Q` keeps allocated. `None` once the take is settled.
    pub fn deliver_next<'b>(
        &mut self,
        resolve: impl FnOnce(NonNull<WakeBridge>) -> &'b WakeBridge,
    ) -> Option<(NonNull<WakeBridge>, Delivery)> {
        let at = NonNull::new(self.next)?;
        let old = self.step(at, resolve);
        debug_assert!(!old.has(CLOSED), "a live executor's headers are open");
        Some((at, if old.has(WOKEN) { Delivery::Wake } else { Delivery::NoWake }))
    }

    /// Settles the next bridge of the last take, whose headers are all closed: reads its link,
    /// then clears its `Q`, and says whether that left the header free. `None` once settled.
    pub fn settle_next<'b>(
        &mut self,
        resolve: impl FnOnce(NonNull<WakeBridge>) -> &'b WakeBridge,
    ) -> Option<(NonNull<WakeBridge>, Settled)> {
        let at = NonNull::new(self.next)?;
        let old = self.step(at, resolve);
        debug_assert!(old.has(CLOSED), "the last take follows every close");
        let settled = if old.count() == 0 { Settled::Last } else { Settled::Kept };
        Some((at, settled))
    }

    /// The link first, then the clear: once `Q` is clear another thread may push the bridge
    /// again, which rewrites its link (ADR-0003 A1.2.7).
    fn step<'b>(
        &mut self,
        at: NonNull<WakeBridge>,
        resolve: impl FnOnce(NonNull<WakeBridge>) -> &'b WakeBridge,
    ) -> Lane {
        let bridge = resolve(at);
        debug_assert!(ptr::eq(bridge, at.as_ptr()), "resolve returns the bridge at its address");
        bridge.tomb.visit();
        // Relaxed: the take's acquire made the push's write visible.
        self.next = bridge.link.load(Ordering::Relaxed);
        bridge.clear()
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    //! The bridge's contract on one thread at a time, its marks, and the bound's fail-stop. The
    //! interleavings are the Loom models' (`loom_tests.rs`).

    use std::sync::mpsc;

    use super::*;

    const RELAXED: Ordering = Ordering::Relaxed;

    /// Settles a live executor's take: the deliveries, in stack order.
    fn deliveries(stack: &RemoteStack, bridge: &WakeBridge) -> Vec<Delivery> {
        let mut drained = stack.take();
        let mut found = Vec::new();
        while let Some((at, delivery)) = drained.deliver_next(|_| bridge) {
            assert!(ptr::eq(at.as_ptr(), bridge), "the take holds this test's bridge");
            found.push(delivery);
        }
        found
    }

    #[test]
    fn two_threads_claim_distinct_marks_neither_zero_nor_closed() {
        let here = ThreadMark::claim();
        assert_eq!(ThreadMark::claim(), here, "a thread claims its mark once");
        assert_eq!(ThreadMark::current(), here.get());
        let there = std::thread::spawn(|| {
            assert_eq!(ThreadMark::current(), 0, "a thread that claimed nothing reads 0");
            ThreadMark::claim()
        })
        .join()
        .expect("the other thread claims a mark");
        assert_ne!(here, there, "two threads, two marks");
        for mark in [here, there] {
            assert_ne!(mark.get(), 0, "0 names a thread with no mark");
            assert_ne!(mark.get(), MARK_CLOSED, "MARK_CLOSED is no thread's");
        }
    }

    /// A waker dropped by another thread local's destructor reads the thread's mark: the mark is a
    /// `const` thread local with no `Drop`, so it outlives every destructor of its thread.
    #[test]
    fn a_mark_is_readable_while_thread_locals_are_destroyed() {
        struct ReportOnDrop(Option<mpsc::Sender<u64>>);
        impl Drop for ReportOnDrop {
            fn drop(&mut self) {
                if let Some(to) = self.0.take() {
                    let _ = to.send(ThreadMark::current());
                }
            }
        }
        std::thread_local! {
            static REPORT: Cell<Option<ReportOnDrop>> = const { Cell::new(None) };
        }
        let (to, from) = mpsc::channel();
        let claimed = std::thread::spawn(move || {
            let mark = ThreadMark::claim();
            REPORT.with(|slot| slot.set(Some(ReportOnDrop(Some(to)))));
            mark
        })
        .join()
        .expect("the thread claims a mark and exits");
        let read = from.recv().expect("the destructor reports the mark it read");
        assert_eq!(read, claimed.get(), "the mark read during destruction is the thread's own");
    }

    #[test]
    fn a_count_at_either_bound_keeps_its_flags() {
        for count in [-LANE_COUNT_MAX, -1, 0, 1, LANE_COUNT_MAX] {
            let lane = Lane(QUEUED | CLOSED).with_count(count);
            assert_eq!(lane.count(), count);
            assert!(
                lane.has(QUEUED) && lane.has(CLOSED) && !lane.has(WOKEN),
                "flags kept at {count}"
            );
        }
        // One past the bound wraps the 61-bit field: the bound's compare is what keeps it out.
        assert_eq!(Lane(0).with_count(LANE_COUNT_MAX + 1).count(), -LANE_COUNT_MAX - 1);
    }

    #[test]
    fn foreign_wakes_queue_the_bridge_once_and_the_drain_delivers_them() {
        let stack = RemoteStack::new();
        let bridge = WakeBridge::new(&stack, ThreadMark::claim());
        assert!(!stack.pending(), "a new stack holds nothing");
        assert_eq!(bridge.foreign_wake_by_ref(&stack), Pushed::Now);
        assert_eq!(bridge.foreign_wake_by_ref(&stack), Pushed::Already, "one entry per header");
        assert!(stack.pending());
        assert_eq!(deliveries(&stack, &bridge), [Delivery::Wake]);
        assert!(!stack.pending(), "the take emptied the stack");
        assert!(bridge.quiet(0), "no waker counted, nothing queued");
        assert_eq!(bridge.foreign_wake_by_ref(&stack), Pushed::Now, "a wake after the drain");
        assert_eq!(deliveries(&stack, &bridge), [Delivery::Wake]);
        assert_eq!(stack.foreign_ops(), 3, "every foreign call is counted");
    }

    #[test]
    fn a_foreign_last_drop_queues_without_a_wake() {
        let stack = RemoteStack::new();
        let bridge = WakeBridge::new(&stack, ThreadMark::claim());
        // A clone the owner lane counted (o = 1), dropped on the foreign lane: c = −1, w = 0.
        assert_eq!(bridge.foreign_drop(&stack), Released::Queued);
        assert!(!bridge.quiet(1), "queued: not quiet until the drain clears it");
        assert_eq!(deliveries(&stack, &bridge), [Delivery::NoWake]);
        assert!(bridge.quiet(1), "w = o + c = 0 and nothing queued");
        // A foreign clone and its foreign wake: c back to −1, a wake this time.
        bridge.foreign_clone(&stack);
        assert_eq!(bridge.foreign_wake(&stack), Released::Queued);
        assert_eq!(deliveries(&stack, &bridge), [Delivery::Wake]);
        assert!(bridge.quiet(1));
    }

    #[test]
    fn close_frees_a_quiet_header_and_retains_a_named_one() {
        let stack = RemoteStack::new();
        let quiet = WakeBridge::new(&stack, ThreadMark::claim());
        assert_eq!(quiet.close(0), Closing::Free);
        assert_eq!(quiet.owner_mark(), MARK_CLOSED, "a closed header has no owner lane");
        let named = WakeBridge::new(&stack, ThreadMark::claim());
        assert_eq!(named.close(1), Closing::Retain, "one waker the owner lane counted");
        assert_eq!(
            named.foreign_wake_by_ref(&stack),
            Pushed::Closed,
            "a closed header's wake is inert"
        );
        named.foreign_clone(&stack);
        assert_eq!(named.foreign_drop(&stack), Released::Kept, "one waker left");
        assert!(!stack.pending(), "a closed header pushes nothing");
        assert_eq!(named.foreign_drop(&stack), Released::Last, "the last drop frees it");
    }

    #[test]
    fn the_last_take_settles_a_queued_closed_header() {
        let stack = RemoteStack::new();
        let bridge = WakeBridge::new(&stack, ThreadMark::claim());
        // A foreign drop of the owner lane's one clone queues the header; then the executor drops.
        assert_eq!(bridge.foreign_drop(&stack), Released::Queued);
        stack.retain();
        assert_eq!(bridge.close(1), Closing::Retain, "w = 0 but queued: the last take decides");
        let mut last = stack.take_last();
        let (at, settled) = last.settle_next(|_| &bridge).expect("the queued bridge");
        assert!(ptr::eq(at.as_ptr(), &bridge));
        assert_eq!(settled, Settled::Last);
        assert!(last.settle_next(|_| &bridge).is_none(), "one entry");
        assert_eq!(stack.release(), Freed::No, "the executor's own count remains");
        assert_eq!(stack.release(), Freed::Stack, "the last count frees the stack");
    }

    #[test]
    fn a_push_after_the_last_take_lands_nowhere() {
        let stack = RemoteStack::new();
        let bridge = WakeBridge::new(&stack, ThreadMark::claim());
        let last = stack.take_last();
        assert!(last.next.is_null(), "nothing was queued");
        // A header whose close the word has not shown: its push finds the head closed, clears its
        // own `Q`, and decides nothing (the close that follows does).
        assert_eq!(bridge.foreign_drop(&stack), Released::Kept);
        assert!(!Lane(bridge.lane.load(RELAXED)).has(QUEUED), "the pusher cleared its own Q");
        assert_eq!(bridge.foreign_wake_by_ref(&stack), Pushed::Closed);
    }
}

#[cfg(all(test, loom))]
mod loom_tests {
    //! The Loom models of the wake bridge's lane (ADR-0003 A1.7), on this module's own types, every
    //! interleaving within `just loom`'s preemption bound. A model's executor is the owner thread's
    //! code; its frees are tombs: a free buries the freed thing's tombs, and every access to a
    //! bridge or a stack visits its tomb first (`Tomb`), so a free not ordered after an access, or
    //! an access after a free, is Loom's causality violation. Each model counts its own frees too.

    use loom::sync::Arc;
    use loom::sync::atomic::AtomicUsize;
    use loom::thread;

    use super::*;

    const RELAXED: Ordering = Ordering::Relaxed;

    /// A model header: the bridge, the owner part's tomb and the header's frees.
    struct Header {
        bridge: WakeBridge,
        owner_part: Tomb,
        frees: AtomicUsize,
    }

    /// A model executor's stack and headers, shared with the model's other threads.
    #[derive(Clone)]
    struct Model {
        stack: Arc<RemoteStack>,
        headers: Arc<Vec<Header>>,
        /// The stack's frees, and the header frees counted when the stack was freed.
        stack_frees: Arc<AtomicUsize>,
        headers_freed_before_the_stack: Arc<AtomicUsize>,
    }

    impl Model {
        fn new(headers: usize) -> Model {
            let stack = Arc::new(RemoteStack::new());
            let mark = ThreadMark::claim();
            let headers = (0..headers)
                .map(|_| Header {
                    bridge: WakeBridge::new(&stack, mark),
                    owner_part: Tomb::new(),
                    frees: AtomicUsize::new(0),
                })
                .collect();
            Model {
                stack,
                headers: Arc::new(headers),
                stack_frees: Arc::new(AtomicUsize::new(0)),
                headers_freed_before_the_stack: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn bridge(&self, index: usize) -> &WakeBridge {
            &self.headers[index].bridge
        }

        /// The header whose bridge is at `at`; a link to anything else is a stale link.
        fn index(&self, at: NonNull<WakeBridge>) -> usize {
            self.headers
                .iter()
                .position(|header| ptr::eq(&header.bridge, at.as_ptr()))
                .expect("drained-once: a link names no bridge of the model")
        }

        fn resolve(&self, at: NonNull<WakeBridge>) -> &WakeBridge {
            self.bridge(self.index(at))
        }

        /// Frees header `index`: both parts.
        fn free_header(&self, index: usize) {
            let header = &self.headers[index];
            header.bridge.tomb.bury();
            header.owner_part.bury();
            header.frees.fetch_add(1, RELAXED);
        }

        /// Frees closed header `index`, then releases the stack's count of it.
        fn free_closed(&self, index: usize) {
            self.free_header(index);
            self.release_stack();
        }

        fn release_stack(&self) {
            match self.stack.release() {
                Freed::No => {}
                Freed::Stack => {
                    self.stack.tomb.bury();
                    self.stack_frees.fetch_add(1, RELAXED);
                    let freed = self.headers.iter().map(|header| header.frees.load(RELAXED)).sum();
                    self.headers_freed_before_the_stack.store(freed, RELAXED);
                }
            }
        }

        /// One live drain: each taken bridge settled once, at most one entry per header (`Q`). Adds
        /// each header's settles to `drained` and returns the wakes delivered. The executor gates
        /// its take with `pending`, whose relaxed load only defers a drain; the model takes every
        /// time, since Loom explores no schedule in which that load reads a push.
        fn drain(&self, drained: &mut [u32]) -> u32 {
            let mut taken = self.stack.take();
            let mut seen = vec![false; self.headers.len()];
            let mut woken = 0;
            while let Some((at, delivery)) = taken.deliver_next(|at| self.resolve(at)) {
                let index = self.index(at);
                assert!(
                    !seen[index],
                    "drained-once: bridge {index} twice in one take (a stale link)"
                );
                seen[index] = true;
                drained[index] += 1;
                match delivery {
                    Delivery::Wake => woken += 1,
                    Delivery::NoWake => {}
                }
            }
            woken
        }

        /// The executor's drop from its close on: every header not yet freed is counted on the
        /// stack, its owner part dropped in place and closed (`owner` holds each one's `o`), then
        /// the last take, then the executor's own count.
        fn close_and_take_last(&self, owner: &[i64], freed: &[bool]) {
            for (index, header) in self.headers.iter().enumerate() {
                if freed[index] {
                    continue;
                }
                self.stack.retain();
                header.owner_part.bury();
                match header.bridge.close(owner[index]) {
                    Closing::Free => self.free_closed(index),
                    Closing::Retain => {}
                }
            }
            let mut last = self.stack.take_last();
            while let Some((at, settled)) = last.settle_next(|at| self.resolve(at)) {
                match settled {
                    Settled::Last => self.free_closed(self.index(at)),
                    Settled::Kept => {}
                }
            }
            self.release_stack();
        }

        /// A foreign thread's drop of its waker of header `index`, freeing it if it was the last.
        fn foreign_drop(&self, index: usize) -> Released {
            let released = self.bridge(index).foreign_drop(&self.stack);
            match released {
                Released::Last => self.free_closed(index),
                Released::Queued | Released::Kept => {}
            }
            released
        }

        /// After every thread is joined: each header freed once, then the stack once, after them.
        fn assert_freed_once(&self) {
            for (index, header) in self.headers.iter().enumerate() {
                assert_eq!(header.frees.load(RELAXED), 1, "header {index} freed once");
            }
            assert_eq!(self.stack_frees.load(RELAXED), 1, "the stack freed once");
            assert_eq!(
                self.headers_freed_before_the_stack.load(RELAXED),
                self.headers.len(),
                "the stack freed after every header it counts"
            );
        }
    }

    /// Two foreign wakes of one header against a drain: never two entries in one take, and the
    /// wake delivered at least once. A wake that sets `W` without `Q` pushes nothing and is lost.
    #[test]
    fn loom_two_foreign_wakes_against_a_drain_are_delivered_once() {
        loom::model(|| {
            let model = Model::new(1);
            let wakers: Vec<_> = (0..2)
                .map(|_| {
                    let model = model.clone();
                    thread::spawn(move || model.bridge(0).foreign_wake_by_ref(&model.stack))
                })
                .collect();
            let mut drained = [0];
            let mut woken = model.drain(&mut drained);
            for waker in wakers {
                let _ = waker.join().expect("a waking thread");
            }
            woken += model.drain(&mut drained);
            assert!(
                woken >= 1,
                "delivered-at-least-once: two foreign wakes were delivered {woken} times"
            );
            assert!(!model.stack.pending(), "the drains left nothing queued");
            assert!(model.bridge(0).quiet(0), "nothing counted, nothing queued");
        });
    }

    /// A foreign `wake_by_ref`, which pushes, then the thread's last drop of the waker the
    /// owner lane counted (`o = 1`), which finds `Q` set and pushes nothing, against the owner's
    /// drain and reuse: the owner reuses the header once, and only after the last foreign access.
    #[test]
    fn loom_a_foreign_last_drop_never_races_the_owners_reuse() {
        loom::model(|| {
            let model = Model::new(1);
            let foreign = {
                let model = model.clone();
                thread::spawn(move || {
                    let _ = model.bridge(0).foreign_wake_by_ref(&model.stack);
                    model.bridge(0).foreign_drop(&model.stack)
                })
            };
            let reuse = |model: &Model| {
                let quiet = model.bridge(0).quiet(1);
                if quiet {
                    model.free_header(0);
                }
                quiet
            };
            let mut drained = [0];
            model.drain(&mut drained);
            let mut reused = reuse(&model);
            let released = foreign.join().expect("the foreign thread");
            assert_ne!(released, Released::Last, "a live header's drop frees nothing itself");
            model.drain(&mut drained);
            if !reused {
                reused = reuse(&model);
            }
            assert!(
                reused,
                "the header is quiet once its last waker dropped and its queue drained"
            );
            assert_eq!(model.headers[0].frees.load(RELAXED), 1, "reused once");
        });
    }

    /// Three pushers, two of one bridge and one of another, against two drains and a final one:
    /// every bridge drained once per `Q` episode (each push that set `Q`), no stale link.
    #[test]
    fn loom_pushes_and_drains_lose_no_bridge() {
        loom::model(|| {
            let model = Model::new(2);
            let pushers: Vec<_> = [0, 0, 1]
                .into_iter()
                .map(|index| {
                    let model = model.clone();
                    thread::spawn(move || {
                        (index, model.bridge(index).foreign_wake_by_ref(&model.stack))
                    })
                })
                .collect();
            let mut drained = [0, 0];
            model.drain(&mut drained);
            model.drain(&mut drained);
            let mut pushed = [0, 0];
            for pusher in pushers {
                let (index, outcome) = pusher.join().expect("a pusher");
                match outcome {
                    Pushed::Now => pushed[index] += 1,
                    Pushed::Already | Pushed::Closed => {}
                }
            }
            model.drain(&mut drained);
            for index in 0..2 {
                assert_eq!(
                    drained[index], pushed[index],
                    "drained-once: bridge {index} was pushed {} times and drained {}",
                    pushed[index], drained[index]
                );
            }
        });
    }

    /// The executor's drop (its drain, the close, the last take, their frees and the release of its
    /// own count) against a foreign thread, spawned first, holding the one waker the owner lane
    /// counted (`o = 1`) of a finished task's header: its `wake_by_ref`, whose push is decided
    /// before `X` and lands before or after the last take, then its last drop, before or after `X`.
    /// The header is freed once, the stack once and after it, and nothing is touched after its
    /// free.
    #[test]
    fn loom_the_executors_drop_never_races_a_foreign_call() {
        loom::model(|| {
            let model = Model::new(1);
            let foreign = {
                let model = model.clone();
                thread::spawn(move || {
                    let _ = model.bridge(0).foreign_wake_by_ref(&model.stack);
                    let _ = model.foreign_drop(0);
                })
            };
            // The drain: the header is Held (its task finished), so a quiet one is freed now.
            let mut taken = model.stack.take();
            let mut freed = [false];
            while let Some((at, _)) = taken.deliver_next(|at| model.resolve(at)) {
                let index = model.index(at);
                if model.bridge(index).quiet(1) {
                    model.free_header(index);
                    freed[index] = true;
                }
            }
            model.close_and_take_last(&[1], &freed);
            foreign.join().expect("the foreign thread");
            model.assert_freed_once();
        });
    }

    /// A header named by two wakers the owner lane counted (`o = 2`), which two threads hold. The
    /// threads are spawned first; then the owner runs the close and the last take against their
    /// drops, so no spawn orders the owner part's drop before a free, and each drop lands before
    /// `X` (it queues the bridge) or after it. The header is freed once, the stack once and after
    /// it.
    #[test]
    fn loom_a_closed_headers_last_drops_free_it_once() {
        loom::model(|| {
            let model = Model::new(1);
            let droppers: Vec<_> = (0..2)
                .map(|_| {
                    let model = model.clone();
                    thread::spawn(move || {
                        let _ = model.foreign_drop(0);
                    })
                })
                .collect();
            model.close_and_take_last(&[2], &[false]);
            for dropper in droppers {
                dropper.join().expect("a dropping thread");
            }
            model.assert_freed_once();
        });
    }
}
