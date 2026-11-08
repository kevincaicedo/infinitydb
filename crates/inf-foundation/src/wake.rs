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
//! **Orderings.** Each load-bearing ordering is a named const; every other access is `Relaxed`,
//! with its reason at its use.

use core::cell::Cell;
use core::ptr::{self, NonNull};

use crate::limits::LANE_COUNT_MAX;

use core::sync::atomic::{AtomicPtr, AtomicU64, Ordering};

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
// operations and count of what keeps it.
const _: () = assert!(size_of::<WakeBridge>() == 32, "a bridge is four words");
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
        let mut word = self.lane.load(Ordering::Relaxed);
        let old = loop {
            let lane = Lane(word);
            if lane.has(CLOSED) {
                return Pushed::Closed;
            }
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
        let mut word = self.lane.load(Ordering::Relaxed);
        loop {
            let new = next(Lane(word));
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
        self.foreign_ops.fetch_add(1, Ordering::Relaxed);
    }

    /// Pushes `bridge`, whose swap set `Q` from clear. Retried only when another thread's push or
    /// take changed the head first (lock-free).
    fn push(&self, bridge: &WakeBridge) -> Landed {
        let node = ptr::from_ref(bridge).cast_mut();
        let mut head = self.head.load(Ordering::Relaxed);
        loop {
            if head == CLOSED_HEAD {
                return Landed::Closed;
            }
            // Relaxed: the head's release publishes it.
            bridge.link.store(head, Ordering::Relaxed);
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
        // Relaxed: the take's acquire made the push's write visible.
        self.next = bridge.link.load(Ordering::Relaxed);
        bridge.clear()
    }
}

#[cfg(test)]
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
