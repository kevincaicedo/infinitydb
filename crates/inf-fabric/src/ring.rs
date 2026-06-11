//! SPSC ring buffer — the fabric's load-bearing primitive (M0-S08).
//!
//! Design: a fixed power-of-two slab of slots; producer and consumer each own
//! a free-running index (wrapping arithmetic, slot = `index & mask`). Indices
//! are published with `Release` and observed with `Acquire` — **no `SeqCst`
//! anywhere**. Each side caches the other's last-seen index so the common
//! case touches only its own `CachePadded` line (false-sharing discipline,
//! master plan §6.1).
//!
//! Batch publication is the throughput lever: [`Producer::publish_batch`]
//! performs one `Release` store for the whole batch, and
//! [`Consumer::consume_batch`] one `Release` store per drain — the `< 40
//! ns/msg` amortized gate is measured against these paths.
//!
//! Verification: the `loom_*` tests model this module under
//! `RUSTFLAGS="--cfg loom"` (publish/consume/wrap-around/full/empty across
//! two threads); the non-loom unit tests run under Miri in CI. The `perf c2c`
//! false-sharing artifact required by M0-S08 is **deferred to the Linux
//! reference box** (`scripts/perf-c2c-ring.sh`); macOS has no equivalent.
//!
//! This is the **only** module in `inf-fabric` allowed to contain `unsafe`
//! code; every block is inventoried in `SAFETY.md`.

use core::mem::MaybeUninit;

use inf_foundation::CachePadded;

/// Atomics/cells, swapped wholesale for loom's model-checked versions under
/// `--cfg loom` (type-alias module pattern).
#[cfg(loom)]
mod sync {
    pub(super) use loom::cell::UnsafeCell;
    pub(super) use loom::sync::Arc;
    pub(super) use loom::sync::atomic::{AtomicUsize, Ordering};
}

#[cfg(not(loom))]
mod sync {
    pub(super) use core::sync::atomic::{AtomicUsize, Ordering};
    pub(super) use std::sync::Arc;

    /// Shim over `core::cell::UnsafeCell` exposing the closure-based slice of
    /// `loom::cell::UnsafeCell` the ring uses, so ring code is identical
    /// under both cfgs.
    #[derive(Debug)]
    pub(super) struct UnsafeCell<T>(core::cell::UnsafeCell<T>);

    impl<T> UnsafeCell<T> {
        pub(super) fn new(value: T) -> UnsafeCell<T> {
            UnsafeCell(core::cell::UnsafeCell::new(value))
        }

        #[inline]
        pub(super) fn with<R>(&self, f: impl FnOnce(*const T) -> R) -> R {
            f(self.0.get())
        }

        #[inline]
        pub(super) fn with_mut<R>(&self, f: impl FnOnce(*mut T) -> R) -> R {
            f(self.0.get())
        }
    }
}

use sync::Ordering;

/// State shared by the two ring handles.
///
/// Ownership protocol (the basis of every `unsafe` block below):
/// - `tail` is stored only by the producer; `head` only by the consumer.
/// - Slots in `head..tail` are initialized and owned by the consumer.
/// - Slots in `tail..head + capacity` are vacant and owned by the producer.
/// - Transfer producer→consumer happens at `tail.store(Release)` /
///   `tail.load(Acquire)`; consumer→producer at `head.store(Release)` /
///   `head.load(Acquire)`.
struct Shared<T> {
    /// Next index the consumer will read. Single writer: the consumer.
    head: CachePadded<sync::AtomicUsize>,
    /// Next index the producer will write. Single writer: the producer.
    tail: CachePadded<sync::AtomicUsize>,
    /// `capacity - 1`; capacity is a power of two.
    mask: usize,
    slots: Box<[sync::UnsafeCell<MaybeUninit<T>>]>,
}

// SAFETY: `Shared<T>` is accessed by exactly two threads under the ownership
// protocol documented on the struct: the producer mutates only `tail` and
// vacant slots, the consumer mutates only `head` and reads only initialized
// slots, and all slot ownership transfers are ordered by Release/Acquire
// pairs on `tail`/`head`. Values of `T` move across threads, hence `T: Send`.
// No `&T` is ever shared across threads, so `T: Sync` is not required.
unsafe impl<T: Send> Sync for Shared<T> {}

impl<T> Drop for Shared<T> {
    fn drop(&mut self) {
        // Both handles are gone (we are inside the final Arc drop, which
        // synchronizes with all prior handle activity), so this thread has
        // exclusive access to every index and slot.
        let head = self.head.load(Ordering::Acquire);
        let tail = self.tail.load(Ordering::Acquire);
        let mut index = head;
        while index != tail {
            self.slots[index & self.mask].with_mut(|slot| {
                // SAFETY: slots in `head..tail` hold initialized, unconsumed
                // values (ownership protocol above) and are dropped exactly
                // once here; nothing reads them afterwards.
                unsafe { (*slot).assume_init_drop() }
            });
            index = index.wrapping_add(1);
        }
    }
}

/// Write half of an SPSC ring. `Send` (move it to the producing thread),
/// single-owner: all methods take `&mut self`.
pub struct Producer<T> {
    shared: sync::Arc<Shared<T>>,
    /// Local copy of `shared.tail` (we are its only writer).
    tail: usize,
    /// Last observed consumer index; refreshed only when the ring looks full.
    head_cache: usize,
}

/// Read half of an SPSC ring. `Send`, single-owner.
pub struct Consumer<T> {
    shared: sync::Arc<Shared<T>>,
    /// Local copy of `shared.head` (we are its only writer).
    head: usize,
    /// Last observed producer index; refreshed only when the ring looks empty.
    tail_cache: usize,
}

/// Creates a bounded SPSC ring of `capacity` slots.
///
/// The handles are `Send` (for `T: Send`): move the [`Producer`] to the
/// sending thread and the [`Consumer`] to the receiving thread. Unconsumed
/// items are dropped when both handles are gone.
///
/// # Panics
///
/// Panics if `capacity` is zero or not a power of two.
pub fn ring<T>(capacity: usize) -> (Producer<T>, Consumer<T>) {
    assert!(
        capacity.is_power_of_two(),
        "ring capacity must be a non-zero power of two, got {capacity}"
    );
    let slots: Box<[sync::UnsafeCell<MaybeUninit<T>>]> =
        (0..capacity).map(|_| sync::UnsafeCell::new(MaybeUninit::uninit())).collect();
    let shared = sync::Arc::new(Shared {
        head: CachePadded(sync::AtomicUsize::new(0)),
        tail: CachePadded(sync::AtomicUsize::new(0)),
        mask: capacity - 1,
        slots,
    });
    let producer = Producer { shared: sync::Arc::clone(&shared), tail: 0, head_cache: 0 };
    let consumer = Consumer { shared, head: 0, tail_cache: 0 };
    (producer, consumer)
}

impl<T> Producer<T> {
    /// Ring capacity in slots.
    #[inline]
    pub fn capacity(&self) -> usize {
        self.shared.mask + 1
    }

    /// Free slots from the producer's view, refreshing the cached consumer
    /// index at most once (only when the cached view cannot satisfy `want`).
    #[inline]
    fn free_slots(&mut self, want: usize) -> usize {
        let cap = self.shared.mask + 1;
        let free = cap - self.tail.wrapping_sub(self.head_cache);
        if free >= want {
            return free;
        }
        self.head_cache = self.shared.head.load(Ordering::Acquire);
        cap - self.tail.wrapping_sub(self.head_cache)
    }

    /// Moves `value` into the slot at `index` (masked).
    ///
    /// # Safety
    ///
    /// `index` must lie in the producer-owned vacant range
    /// `published_tail..head + capacity` and must not have been written since
    /// it was last consumed — i.e. the slot is vacant and this producer is
    /// the only thread touching it.
    #[inline]
    unsafe fn write_slot(&self, index: usize, value: T) {
        self.shared.slots[index & self.shared.mask].with_mut(|slot| {
            // SAFETY: caller contract — the slot is vacant and exclusively
            // ours; writing a fresh `MaybeUninit` does not drop the (absent)
            // previous value.
            unsafe { slot.write(MaybeUninit::new(value)) };
        });
    }

    /// Pushes one value, publishing it immediately (one `Release` store).
    ///
    /// # Errors
    ///
    /// Returns `Err(value)` if the ring is full; the value is handed back and
    /// the ring is unchanged.
    #[inline]
    pub fn try_push(&mut self, value: T) -> Result<(), T> {
        if self.free_slots(1) == 0 {
            return Err(value);
        }
        // SAFETY: `free_slots(1) > 0` means `self.tail` is in the vacant
        // producer-owned range; it is not yet published, so the consumer
        // cannot observe it.
        unsafe { self.write_slot(self.tail, value) };
        self.tail = self.tail.wrapping_add(1);
        self.shared.tail.store(self.tail, Ordering::Release);
        Ok(())
    }

    /// Writes as many items as fit, then publishes them with a **single**
    /// `Release` store. Returns the number published.
    ///
    /// Items are pulled from `it` only when a slot is available, so passing
    /// `iter.by_ref()` retains unpublished items for a later retry.
    pub fn publish_batch(&mut self, mut it: impl Iterator<Item = T>) -> usize {
        // A batch wants maximum room: refresh the consumer index once.
        let free = self.free_slots(usize::MAX);
        let mut n = 0;
        while n < free {
            let Some(value) = it.next() else { break };
            // SAFETY: `n < free` keeps `tail + n` inside the vacant
            // producer-owned range established by `free_slots`; none of these
            // slots are published until the store below.
            unsafe { self.write_slot(self.tail.wrapping_add(n), value) };
            n += 1;
        }
        if n > 0 {
            self.tail = self.tail.wrapping_add(n);
            self.shared.tail.store(self.tail, Ordering::Release);
        }
        n
    }
}

impl<T> Consumer<T> {
    /// Ring capacity in slots.
    #[inline]
    pub fn capacity(&self) -> usize {
        self.shared.mask + 1
    }

    /// Moves the value out of the slot at `index` (masked).
    ///
    /// # Safety
    ///
    /// `index` must lie in `head..tail_published` (an initialized,
    /// unconsumed slot observed via an `Acquire` load of `tail`) and must not
    /// be read again afterwards without an intervening producer write.
    #[inline]
    unsafe fn read_slot(&self, index: usize) -> T {
        self.shared.slots[index & self.shared.mask].with(|slot| {
            // SAFETY: caller contract — the slot is initialized (the
            // `Acquire` load of `tail` ordered the producer's write before
            // this read) and is consumed exactly once.
            unsafe { (*slot).assume_init_read() }
        })
    }

    /// Consumes up to `max` items, handing each to `f`, then publishes the
    /// new consumer index with a **single** `Release` store. Returns the
    /// number consumed (0 when the ring is empty).
    ///
    /// Progress is published even if `f` panics, so a panicking callback
    /// never causes an item to be consumed twice.
    pub fn consume_batch(&mut self, max: usize, mut f: impl FnMut(T)) -> usize {
        let mut available = self.tail_cache.wrapping_sub(self.head);
        if available < max {
            // Cached view can't satisfy the request: refresh once (mirrors
            // the producer's lazy `free_slots`). At most one Acquire load
            // per call, amortized over the whole batch.
            self.tail_cache = self.shared.tail.load(Ordering::Acquire);
            available = self.tail_cache.wrapping_sub(self.head);
        }
        if available == 0 {
            return 0;
        }
        let n = available.min(max);
        let guard = AdvanceGuard { consumer: self };
        for _ in 0..n {
            let index = guard.consumer.head;
            // SAFETY: `index < tail_cache`, which was Acquire-loaded from the
            // producer's published tail, so the slot is initialized; `head`
            // is advanced past it immediately below (before `f` can panic),
            // so it is consumed exactly once.
            let value = unsafe { guard.consumer.read_slot(index) };
            guard.consumer.head = index.wrapping_add(1);
            f(value);
        }
        drop(guard);
        n
    }
}

/// Publishes consumer progress on drop — exactly once per `consume_batch`,
/// including the unwind path.
struct AdvanceGuard<'a, T> {
    consumer: &'a mut Consumer<T>,
}

impl<T> Drop for AdvanceGuard<'_, T> {
    fn drop(&mut self) {
        self.consumer.shared.head.store(self.consumer.head, Ordering::Release);
    }
}

#[cfg(all(test, loom))]
mod loom_tests {
    use super::*;

    /// Cap 2, 3 messages: exercises publish, consume, wrap-around, full and
    /// empty transitions across two threads.
    #[test]
    fn loom_spsc_wraparound_sequence() {
        loom::model(|| {
            let (mut producer, mut consumer) = ring::<u32>(2);
            let join = loom::thread::spawn(move || {
                for value in 0..3u32 {
                    let mut value = value;
                    loop {
                        match producer.try_push(value) {
                            Ok(()) => break,
                            Err(back) => {
                                value = back;
                                loom::thread::yield_now();
                            }
                        }
                    }
                }
            });
            let mut got = Vec::new();
            while got.len() < 3 {
                if consumer.consume_batch(8, |v| got.push(v)) == 0 {
                    loom::thread::yield_now();
                }
            }
            join.join().unwrap();
            assert_eq!(got, [0, 1, 2]);
        });
    }

    /// A whole batch becomes visible atomically-enough: the consumer sees a
    /// prefix in order, never a gap, after the single `Release` store.
    #[test]
    fn loom_publish_batch_single_release() {
        loom::model(|| {
            let (mut producer, mut consumer) = ring::<u32>(4);
            let join = loom::thread::spawn(move || {
                assert_eq!(producer.publish_batch([7u32, 8, 9].into_iter()), 3);
            });
            let mut got = Vec::new();
            while got.len() < 3 {
                if consumer.consume_batch(2, |v| got.push(v)) == 0 {
                    loom::thread::yield_now();
                }
            }
            join.join().unwrap();
            assert_eq!(got, [7, 8, 9]);
        });
    }

    /// Cap 1: producer hits `full` and recovers only after the concurrent
    /// consumer frees the slot; values arrive intact and in order.
    #[test]
    fn loom_full_rejects_then_recovers() {
        loom::model(|| {
            let (mut producer, mut consumer) = ring::<u32>(1);
            let join = loom::thread::spawn(move || {
                producer.try_push(1).unwrap();
                let mut value = 2u32;
                loop {
                    match producer.try_push(value) {
                        Ok(()) => break,
                        Err(back) => {
                            value = back;
                            loom::thread::yield_now();
                        }
                    }
                }
            });
            let mut got = Vec::new();
            while got.len() < 2 {
                if consumer.consume_batch(1, |v| got.push(v)) == 0 {
                    loom::thread::yield_now();
                }
            }
            join.join().unwrap();
            assert_eq!(got, [1, 2]);
        });
    }

    /// Empty-ring consume returns 0 and racing with the first publish never
    /// loses or duplicates the message.
    #[test]
    fn loom_empty_consume_races_first_publish() {
        loom::model(|| {
            let (mut producer, mut consumer) = ring::<u32>(2);
            let join = loom::thread::spawn(move || {
                producer.try_push(42).unwrap();
            });
            let mut got = Vec::new();
            // First call may legitimately observe an empty ring.
            let first = consumer.consume_batch(4, |v| got.push(v));
            assert!(first <= 1);
            while got.len() < 1 {
                if consumer.consume_batch(4, |v| got.push(v)) == 0 {
                    loom::thread::yield_now();
                }
            }
            join.join().unwrap();
            assert_eq!(got, [42]);
        });
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[test]
    #[should_panic(expected = "power of two")]
    fn zero_capacity_panics() {
        let _ = ring::<u8>(0);
    }

    #[test]
    #[should_panic(expected = "power of two")]
    fn non_power_of_two_capacity_panics() {
        let _ = ring::<u8>(12);
    }

    #[test]
    fn full_empty_and_wraparound() {
        let (mut producer, mut consumer) = ring::<u32>(4);
        assert_eq!(producer.capacity(), 4);
        assert_eq!(consumer.consume_batch(8, |_| panic!("empty ring yielded a value")), 0);

        // Cycle well past capacity to cover index wrap-around behavior.
        let mut next_expected = 0u32;
        let mut next_value = 0u32;
        for round in 0..16 {
            let fill = (round % 4) + 1;
            for _ in 0..fill {
                producer.try_push(next_value).unwrap();
                next_value += 1;
            }
            let mut got = Vec::new();
            assert_eq!(consumer.consume_batch(usize::MAX, |v| got.push(v)), fill as usize);
            for v in got {
                assert_eq!(v, next_expected);
                next_expected += 1;
            }
        }

        // Fill to capacity; the next push must hand the value back.
        for i in 0..4 {
            producer.try_push(100 + i).unwrap();
        }
        assert_eq!(producer.try_push(999), Err(999));
        let mut got = Vec::new();
        consumer.consume_batch(usize::MAX, |v| got.push(v));
        assert_eq!(got, [100, 101, 102, 103]);
    }

    #[test]
    fn publish_batch_stops_at_capacity_without_eating_items() {
        let (mut producer, mut consumer) = ring::<u32>(4);
        let mut items = (0..10u32).peekable();
        assert_eq!(producer.publish_batch(items.by_ref()), 4);
        // The 5th item was never pulled from the iterator.
        assert_eq!(items.peek(), Some(&4));

        let mut got = Vec::new();
        assert_eq!(consumer.consume_batch(2, |v| got.push(v)), 2);
        assert_eq!(producer.publish_batch(items.by_ref()), 2);
        assert_eq!(consumer.consume_batch(usize::MAX, |v| got.push(v)), 4);
        assert_eq!(got, [0, 1, 2, 3, 4, 5]);
    }

    #[test]
    fn consume_batch_respects_max() {
        let (mut producer, mut consumer) = ring::<u32>(8);
        assert_eq!(producer.publish_batch(0..6u32), 6);
        let mut got = Vec::new();
        assert_eq!(consumer.consume_batch(4, |v| got.push(v)), 4);
        assert_eq!(consumer.consume_batch(4, |v| got.push(v)), 2);
        assert_eq!(got, [0, 1, 2, 3, 4, 5]);
    }

    struct DropTracker(Arc<AtomicUsize>);

    impl Drop for DropTracker {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn unconsumed_items_drop_exactly_once() {
        let drops = Arc::new(AtomicUsize::new(0));
        let (mut producer, mut consumer) = ring::<DropTracker>(4);
        for _ in 0..3 {
            assert!(producer.try_push(DropTracker(Arc::clone(&drops))).is_ok());
        }
        assert_eq!(consumer.consume_batch(1, drop), 1);
        assert_eq!(drops.load(Ordering::Relaxed), 1);
        drop(producer);
        drop(consumer);
        // The two unconsumed items are dropped with the ring — once each.
        assert_eq!(drops.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn two_thread_stress_sequence_integrity() {
        #[cfg(miri)]
        const MESSAGES: u64 = 500;
        #[cfg(not(miri))]
        const MESSAGES: u64 = 1_000_000;

        let (mut producer, mut consumer) = ring::<u64>(1024);
        let join = std::thread::spawn(move || {
            let mut items = 0..MESSAGES;
            let mut published = 0;
            while published < MESSAGES as usize {
                let n = producer.publish_batch(items.by_ref());
                published += n;
                if n == 0 {
                    std::thread::yield_now();
                }
            }
        });

        let mut expected = 0u64;
        while expected < MESSAGES {
            let n = consumer.consume_batch(256, |v| {
                assert_eq!(v, expected, "out-of-order or duplicated message");
                expected += 1;
            });
            if n == 0 {
                std::thread::yield_now();
            }
        }
        join.join().unwrap();
        assert_eq!(expected, MESSAGES);
    }
}
