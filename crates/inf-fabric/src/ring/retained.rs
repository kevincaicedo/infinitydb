//! The retained ring (ADR-0157): the ring's one SPSC algorithm over a
//! backing that a third owner keeps, for the eight-slot cleanup directions
//! between a cell and the control thread (ADR-0156 D2).
//!
//! HEAD's ring frees its backing at the last handle's drop, on whichever
//! thread that is, with any queued values. The retained ring instead has a
//! backing owner, [`RetainedRing`], which a host keeps in its resource root:
//! it is admitted before any allocation, built in bounded steps that can be
//! refused or cancelled, split once into two endpoints, joined once both
//! endpoints are released, and retired under the caller's work allowance,
//! each queued value handed to the caller's cleanup custody. An endpoint's
//! release is its one `Release` decrement of the header's endpoint count;
//! it frees nothing. The owner joins on an `Acquire` load of zero, which
//! orders every endpoint access before its own (ADR-0157 D3).
//!
//! The owner's phases (ADR-0157 D2, D4):
//!
//! | Phase | Step | Next |
//! |---|---|---|
//! | (unadmitted) | `admit`, a block over the byte bound | refused, nothing allocated |
//! | Building, nothing | reserve the slot block | Building, reserved; refused: Retiring |
//! | Building, reserved | initialize its 8 slots | Building, initialized |
//! | Building, initialized | reserve and write the header | Ready; refused: Retiring |
//! | Building or Ready | `cancel` | Retiring(Cancelled) |
//! | Ready | `split` | Live: one producer, one consumer |
//! | Live | `join`, endpoints left > 0 | Live |
//! | Live | `join`, endpoints left = 0 | Retiring(Released) |
//! | Retiring, values queued | one value to the sink | the same |
//! | Retiring, header, empty | the header freed, its slot block kept | Retiring, slots |
//! | Retiring, slots | the slot block freed | Terminal |
//!
//! Every step but the join takes one unit of the caller's [`RingSteps`]; a
//! drop before Terminal is a fail-stop (`Drop` below).

use core::alloc::Layout;
use core::mem::{self, ManuallyDrop, MaybeUninit};
use core::ptr::NonNull;

use inf_foundation::CachePadded;
use inf_foundation::limits::TIER_RETIRE_JOBS_PER_CELL;

use super::sync::{self, Ordering};
use super::{Backing, Receiver, Sender, Slot};
use crate::limits::RETAINED_RING_BACKING_BYTES_MAX;

/// The retained ring's slots: exactly one a cleanup job position (ADR-0157
/// D1, the foundation's `TIER_RETIRE_JOBS_PER_CELL`).
pub const RETAINED_RING_SLOTS: usize = TIER_RETIRE_JOBS_PER_CELL;

/// The endpoints a split makes: one producer, one consumer.
const ENDPOINTS: usize = 2;

/// An endpoint's release: `Release`, its last access to the header.
#[cfg(not(inf_canary_ring_release_relaxed))]
const RELEASE: Ordering = Ordering::Release;
/// Canary: the release without its ordering. The Loom join models must fail.
#[cfg(inf_canary_ring_release_relaxed)]
const RELEASE: Ordering = Ordering::Relaxed;

/// The owner's join: `Acquire`, before it reads anything an endpoint wrote.
#[cfg(not(inf_canary_ring_join_relaxed))]
const JOIN: Ordering = Ordering::Acquire;
/// Canary: the join without its ordering. The Loom join models must fail.
#[cfg(inf_canary_ring_join_relaxed)]
const JOIN: Ordering = Ordering::Relaxed;

/// The shared header: the indices, the endpoint count and the slot block
/// it holds until its retirement detaches it. One element of a one-element
/// block the owner keeps in place from the split to the join.
struct Header<T> {
    head: CachePadded<sync::AtomicUsize>,
    tail: CachePadded<sync::AtomicUsize>,
    mask: usize,
    /// Endpoints not yet released: two at the split, each endpoint's last
    /// access its one `Release` decrement; it only goes down.
    endpoints: sync::AtomicUsize,
    slots: Vec<Slot<T>>,
    tomb: Tomb,
}

/// The header's custody as the Loom models see it: every endpoint access
/// reads this cell first ([`Tomb::visit`]) and every retirement step on the
/// header writes it ([`Tomb::bury`]), so an endpoint access that the join
/// did not order before the retirement is a causality violation Loom
/// reports, a read of a plain field such as `mask` included, which Loom
/// does not see by itself. Zero-sized outside the models: a visit compiles
/// to nothing.
struct Tomb(sync::UnsafeCell<()>);

impl Tomb {
    fn new() -> Tomb {
        Tomb(sync::UnsafeCell::new(()))
    }

    #[inline]
    fn visit(&self) {
        self.0.with(|_| ());
    }

    #[inline]
    fn bury(&self) {
        self.0.with_mut(|_| ());
    }
}

/// An endpoint's way to its header: the address in the owner's block.
struct HeaderRef<T>(NonNull<Header<T>>);

impl<T> HeaderRef<T> {
    #[inline]
    fn header(&self) -> &Header<T> {
        // SAFETY: the owner holds the header block, never grown, moved or
        // freed, from the split until its join reads an `Acquire` zero of
        // `endpoints`, which needs this endpoint's release (ADR-0157 D3);
        // the owner forms no `&mut` to the header in that interval, and
        // every mutation through this shared borrow is an atomic or a slot's
        // `UnsafeCell` (the ownership protocol on `Shared`).
        let header = unsafe { self.0.as_ref() };
        header.tomb.visit();
        header
    }
}

impl<T> Backing<T> for HeaderRef<T> {
    #[inline]
    fn head(&self) -> &sync::AtomicUsize {
        &self.header().head
    }

    #[inline]
    fn tail(&self) -> &sync::AtomicUsize {
        &self.header().tail
    }

    #[inline]
    fn mask(&self) -> usize {
        self.header().mask
    }

    #[inline]
    fn slots(&self) -> &[Slot<T>] {
        &self.header().slots
    }
}

/// An endpoint's release: its one decrement, after which it touches nothing.
fn release<T>(backing: &HeaderRef<T>) {
    backing.header().endpoints.fetch_sub(1, RELEASE);
}

/// Write half of a retained ring: not `Clone`, `Send` only for `T: Send`;
/// dropping it releases it. Every value enters through a [`Reservation`].
///
/// ```compile_fail,E0277
/// fn clone_it<T: Clone>(_: &T) {}
/// fn check(producer: &inf_fabric::RetainedProducer<u64>) {
///     clone_it(producer);
/// }
/// ```
///
/// ```compile_fail,E0277
/// fn send_it<T: Send>(_: T) {}
/// fn check(producer: inf_fabric::RetainedProducer<std::rc::Rc<u8>>) {
///     send_it(producer);
/// }
/// ```
pub struct RetainedProducer<T> {
    inner: Sender<T, HeaderRef<T>>,
}

/// Read half of a retained ring: not `Clone`, `Send` only for `T: Send`;
/// dropping it releases it.
///
/// ```compile_fail,E0277
/// fn send_it<T: Send>(_: T) {}
/// fn check(consumer: inf_fabric::RetainedConsumer<std::rc::Rc<u8>>) {
///     send_it(consumer);
/// }
/// ```
pub struct RetainedConsumer<T> {
    inner: Receiver<T, HeaderRef<T>>,
}

// SAFETY: an endpoint reaches its header only through `HeaderRef`, valid
// until its release (above); the two endpoints touch disjoint state under
// the ownership protocol on `Shared`, so only values of `T` move between
// the threads (by move, never `&T`): `T: Send` and no `T: Sync`.
unsafe impl<T: Send> Send for RetainedProducer<T> {}
// SAFETY: as for `RetainedProducer`.
unsafe impl<T: Send> Send for RetainedConsumer<T> {}

impl<T> Drop for RetainedProducer<T> {
    fn drop(&mut self) {
        // Canary `inf_canary_ring_release_before_write`: the producer
        // released at its publication, before the tail store.
        if cfg!(inf_canary_ring_release_before_write) {
            return;
        }
        release(&self.inner.backing);
        // Canaries `inf_canary_ring_slot_after_release` and
        // `inf_canary_ring_header_after_release`: a slot access, or a read
        // of the header's own field, after the release.
        if cfg!(inf_canary_ring_slot_after_release) {
            self.inner.backing.slots()[1].with_mut(|_| ());
        }
        if cfg!(inf_canary_ring_header_after_release) {
            core::hint::black_box(self.inner.backing.mask());
        }
    }
}

impl<T> Drop for RetainedConsumer<T> {
    fn drop(&mut self) {
        release(&self.inner.backing);
    }
}

impl<T> RetainedProducer<T> {
    /// Ring capacity in slots: [`RETAINED_RING_SLOTS`].
    #[inline]
    pub fn capacity(&self) -> usize {
        self.inner.capacity()
    }

    /// Reserves up to `want` vacant slots before any value moves (ADR-0157
    /// D1): a full ring grants none, and a value is written only with one
    /// of the reservation's permits, so a caller takes a value from its
    /// owner only once a slot holds room for it.
    #[inline]
    pub fn reserve(&mut self, want: usize) -> Reservation<'_, T> {
        let granted = self.inner.free_slots(want).min(want);
        Reservation { sender: &mut self.inner, granted, written: 0 }
    }
}

impl<T> RetainedConsumer<T> {
    /// Ring capacity in slots: [`RETAINED_RING_SLOTS`].
    #[inline]
    pub fn capacity(&self) -> usize {
        self.inner.capacity()
    }

    /// Consumes up to `max` values, handing each to `f` by value, then
    /// publishes the new consumer index with one `Release` store; HEAD's
    /// [`Consumer::consume_batch`](super::Consumer::consume_batch).
    #[inline]
    pub fn consume_batch(&mut self, max: usize, f: impl FnMut(T)) -> usize {
        self.inner.consume_batch(max, f)
    }
}

/// Slots reserved on a retained ring, a synchronous borrow of its producer.
/// Dropping it publishes what was written with one `Release` store, so a
/// written value is never left unpublished; nothing it publishes can be
/// refused.
#[must_use = "a reservation publishes what it wrote when it is dropped"]
pub struct Reservation<'a, T> {
    sender: &'a mut Sender<T, HeaderRef<T>>,
    granted: usize,
    written: usize,
}

impl<'a, T> Reservation<'a, T> {
    /// Slots granted.
    #[must_use]
    pub fn granted(&self) -> usize {
        self.granted
    }

    /// Slots granted and not yet written.
    #[must_use]
    pub fn remaining(&self) -> usize {
        self.granted - self.written
    }

    /// The next slot's permit, or `None` when every granted slot is written.
    pub fn permit(&mut self) -> Option<Permit<'_, 'a, T>> {
        if self.written == self.granted {
            return None;
        }
        Some(Permit { reservation: self })
    }

    /// Publishes what was written (one `Release` store); returns its count.
    pub fn publish(self) -> usize {
        self.written
    }
}

impl<T> Drop for Reservation<'_, T> {
    fn drop(&mut self) {
        if self.written == 0 {
            return;
        }
        // Canary `inf_canary_ring_release_before_write`: the producer's
        // release before its final write, the tail store.
        if cfg!(inf_canary_ring_release_before_write) {
            release(&self.sender.backing);
        }
        self.sender.publish(self.written);
    }
}

/// Room for one value in a [`Reservation`]: writing it cannot refuse.
pub struct Permit<'r, 'a, T> {
    reservation: &'r mut Reservation<'a, T>,
}

impl<T> Permit<'_, '_, T> {
    /// Moves `value` into the reserved slot; the reservation's drop
    /// publishes it.
    pub fn write(self, value: T) {
        let reservation = self.reservation;
        let index = reservation.sender.tail.wrapping_add(reservation.written);
        // SAFETY: a permit exists only while `written < granted`, and
        // `granted` slots past the published tail were vacant and
        // producer-owned at the reservation (`free_slots`); none of them is
        // published before the reservation's drop.
        unsafe { reservation.sender.write_slot(index, value) };
        reservation.written += 1;
    }
}

/// The caller's allowance of primitive steps (ADR-0157 D4): each step of
/// [`RetainedRing::build_step`] and [`RetainedRing::retire_step`] takes one
/// unit, to reserve or free one block, initialize the eight slots, or move
/// one value; one allowance shared by both calls is not reset by either.
#[derive(Debug)]
pub struct RingSteps {
    left: u32,
}

impl RingSteps {
    /// An allowance of `steps` primitive steps.
    #[must_use]
    pub const fn new(steps: u32) -> RingSteps {
        RingSteps { left: steps }
    }

    /// Steps left.
    #[must_use]
    pub fn left(&self) -> u32 {
        self.left
    }

    fn take(&mut self) -> bool {
        let Some(left) = self.left.checked_sub(1) else { return false };
        self.left = left;
        true
    }
}

/// Where a retained ring's owner stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RingPhase {
    /// Admitted, its blocks being built.
    Building,
    /// Built, not yet split.
    Ready,
    /// Split: its endpoints are out.
    Live,
    /// Its values and blocks retire a step at a time.
    Retiring(RetireCause),
    /// Nothing left: it may be dropped.
    Terminal(RetireCause),
}

/// Why a retained ring retires.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetireCause {
    /// The host cancelled it before the split.
    Cancelled,
    /// A block's allocation was refused while building.
    AllocationRefused,
    /// Both endpoints were released and joined.
    Released,
}

/// A retained ring's allocation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RingBlock {
    /// The eight slots.
    Slots,
    /// The shared header.
    Header,
}

/// A layout over [`RETAINED_RING_BACKING_BYTES_MAX`], refused before any
/// allocation; `bytes` is `None` when the size does not fit a `usize`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RingLayoutRefused {
    pub block: RingBlock,
    pub bytes: Option<usize>,
}

impl core::fmt::Display for RingLayoutRefused {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let block = match self.block {
            RingBlock::Slots => "slot block",
            RingBlock::Header => "header",
        };
        match self.bytes {
            Some(bytes) => write!(
                f,
                "retained ring {block} of {bytes} bytes is over {RETAINED_RING_BACKING_BYTES_MAX}"
            ),
            None => write!(f, "retained ring {block} size overflows"),
        }
    }
}

impl std::error::Error for RingLayoutRefused {}

/// What a Building owner holds.
enum Built<T> {
    Nothing,
    /// The slot block, reserved; no slot initialized.
    Reserved(Vec<Slot<T>>),
    /// The slot block with its eight slots initialized.
    Initialized(Vec<Slot<T>>),
}

/// What a Retiring owner still holds.
enum Blocks<T> {
    Nothing,
    Slots(Vec<Slot<T>>),
    /// The header block (one element), holding the slot block.
    Header(Vec<Header<T>>),
}

enum Phase<T> {
    Building(Built<T>),
    Ready(Vec<Header<T>>),
    Live(Vec<Header<T>>),
    Retiring(RetireCause, Blocks<T>),
    Terminal(RetireCause),
}

/// The backing owner of a retained ring (ADR-0157 D2–D4): its blocks, its
/// one split and its retirement. A host keeps it in its resource root until
/// [`RingPhase::Terminal`]; dropped before that, it fails stop and frees
/// nothing (`Drop`).
pub struct RetainedRing<T> {
    phase: ManuallyDrop<Phase<T>>,
}

/// `bytes` within [`RETAINED_RING_BACKING_BYTES_MAX`], else refused.
fn within(block: RingBlock, bytes: usize) -> Result<usize, RingLayoutRefused> {
    if bytes > RETAINED_RING_BACKING_BYTES_MAX {
        return Err(RingLayoutRefused { block, bytes: Some(bytes) });
    }
    Ok(bytes)
}

/// The two blocks' requested bytes, each checked against the bound.
fn block_bytes<T>() -> Result<(usize, usize), RingLayoutRefused> {
    let slots = Layout::array::<Slot<T>>(RETAINED_RING_SLOTS)
        .map_err(|_| RingLayoutRefused { block: RingBlock::Slots, bytes: None })?;
    let slots = within(RingBlock::Slots, slots.size())?;
    let header = within(RingBlock::Header, Layout::new::<Header<T>>().size())?;
    Ok((slots, header))
}

impl<T> Built<T> {
    fn into_blocks(self) -> Blocks<T> {
        match self {
            Built::Nothing => Blocks::Nothing,
            Built::Reserved(slots) | Built::Initialized(slots) => Blocks::Slots(slots),
        }
    }

    /// One construction step; a refused allocation retires what was built.
    fn step(self) -> Phase<T> {
        match self {
            Built::Nothing => {
                let mut slots = Vec::new();
                match slots.try_reserve_exact(RETAINED_RING_SLOTS) {
                    Ok(()) => Phase::Building(Built::Reserved(slots)),
                    Err(_) => Phase::Retiring(RetireCause::AllocationRefused, Blocks::Nothing),
                }
            }
            Built::Reserved(mut slots) => {
                // Within the exact reservation: no push reallocates.
                debug_assert!(slots.capacity() >= RETAINED_RING_SLOTS, "the block is reserved");
                for _ in 0..RETAINED_RING_SLOTS {
                    slots.push(sync::UnsafeCell::new(MaybeUninit::uninit()));
                }
                Phase::Building(Built::Initialized(slots))
            }
            Built::Initialized(slots) => {
                let mut headers = Vec::new();
                if headers.try_reserve_exact(1).is_err() {
                    return Phase::Retiring(RetireCause::AllocationRefused, Blocks::Slots(slots));
                }
                headers.push(Header {
                    head: CachePadded(sync::AtomicUsize::new(0)),
                    tail: CachePadded(sync::AtomicUsize::new(0)),
                    mask: RETAINED_RING_SLOTS - 1,
                    endpoints: sync::AtomicUsize::new(ENDPOINTS),
                    slots,
                    tomb: Tomb::new(),
                });
                Phase::Ready(headers)
            }
        }
    }
}

/// Moves the next queued value out of a header no endpoint holds: joined,
/// or never split. The join's `Acquire` ordered every endpoint access
/// before these loads, so they need no ordering of their own.
fn take_queued<T>(header: &mut Header<T>) -> Option<T> {
    let head = header.head.load(Ordering::Relaxed);
    let tail = header.tail.load(Ordering::Relaxed);
    if head == tail {
        return None;
    }
    let value = header.slots[head & header.mask].with(|slot| {
        // SAFETY: `head..tail` holds initialized, unconsumed values (the
        // ownership protocol on `Shared`), no endpoint remains to read or
        // write them, and `head` moves past this one before the value
        // leaves, so it is moved out exactly once.
        unsafe { (*slot).assume_init_read() }
    });
    header.head.store(head.wrapping_add(1), Ordering::Relaxed);
    Some(value)
}

/// Whether `left` endpoints let the owner join.
fn joined(left: usize) -> bool {
    // Canary `inf_canary_ring_join_after_one`: a join after one release.
    if cfg!(inf_canary_ring_join_after_one) { left <= 1 } else { left == 0 }
}

impl<T> RetainedRing<T> {
    /// Admits a ring of `T`: both blocks' layouts within the byte bound,
    /// checked before anything is allocated (ADR-0157 D1).
    ///
    /// # Errors
    ///
    /// [`RingLayoutRefused`]: a block over the bound, or a size that
    /// overflows; nothing was allocated.
    pub fn admit() -> Result<RetainedRing<T>, RingLayoutRefused> {
        block_bytes::<T>()?;
        Ok(RetainedRing { phase: ManuallyDrop::new(Phase::Building(Built::Nothing)) })
    }

    /// The bytes the ring's two blocks request from the allocator: its slot
    /// block (none for a zero-sized `T`) and its header.
    #[must_use]
    pub fn requested_bytes(&self) -> usize {
        // Admission checked both within the bound, so the sum fits.
        block_bytes::<T>().map_or(0, |(slots, header)| slots + header)
    }

    /// Where the owner stands.
    #[must_use]
    pub fn phase(&self) -> RingPhase {
        match &*self.phase {
            Phase::Building(_) => RingPhase::Building,
            Phase::Ready(_) => RingPhase::Ready,
            Phase::Live(_) => RingPhase::Live,
            Phase::Retiring(cause, _) => RingPhase::Retiring(*cause),
            Phase::Terminal(cause) => RingPhase::Terminal(*cause),
        }
    }

    /// Takes the phase out, leaving a placeholder the caller replaces.
    fn take_phase(&mut self) -> Phase<T> {
        mem::replace(&mut *self.phase, Phase::Terminal(RetireCause::Released))
    }

    /// One construction step, when the owner is Building and `steps` has a
    /// unit: reserve the slot block, initialize its slots, or reserve and
    /// write the header. A refused allocation enters Retiring with
    /// [`RetireCause::AllocationRefused`], what was built kept for it.
    pub fn build_step(&mut self, steps: &mut RingSteps) -> RingPhase {
        if !matches!(*self.phase, Phase::Building(_)) || !steps.take() {
            return self.phase();
        }
        let next = match self.take_phase() {
            Phase::Building(built) => built.step(),
            other @ (Phase::Ready(_)
            | Phase::Live(_)
            | Phase::Retiring(..)
            | Phase::Terminal(_)) => other,
        };
        *self.phase = next;
        self.phase()
    }

    /// Cancels a ring not yet split: it enters Retiring with
    /// [`RetireCause::Cancelled`], what was built kept for it. A Live ring
    /// is unchanged: its host releases its endpoints and joins first.
    pub fn cancel(&mut self) -> RingPhase {
        let next = match self.take_phase() {
            Phase::Building(built) => Phase::Retiring(RetireCause::Cancelled, built.into_blocks()),
            Phase::Ready(headers) => {
                Phase::Retiring(RetireCause::Cancelled, Blocks::Header(headers))
            }
            other @ (Phase::Live(_) | Phase::Retiring(..) | Phase::Terminal(_)) => other,
        };
        *self.phase = next;
        self.phase()
    }

    /// The one split of a Ready ring: its producer and its consumer, made
    /// without allocating; `None` in any other phase, so a ring is split at
    /// most once.
    pub fn split(&mut self) -> Option<(RetainedProducer<T>, RetainedConsumer<T>)> {
        let Phase::Ready(headers) = &*self.phase else { return None };
        // `as_ptr` forms no reference to the element, and the block never
        // moves while the owner holds it.
        let header = NonNull::new(headers.as_ptr().cast_mut())?;
        let next = match self.take_phase() {
            Phase::Ready(headers) => Phase::Live(headers),
            other @ (Phase::Building(_)
            | Phase::Live(_)
            | Phase::Retiring(..)
            | Phase::Terminal(_)) => other,
        };
        *self.phase = next;
        let producer = RetainedProducer { inner: Sender::new(HeaderRef(header)) };
        let consumer = RetainedConsumer { inner: Receiver::new(HeaderRef(header)) };
        Some((producer, consumer))
    }

    /// Joins a Live ring once both endpoints are released: one `Acquire`
    /// load of the endpoint count, nothing else read; zero enters Retiring
    /// with [`RetireCause::Released`]. Not a step: it waits on the host's
    /// terminal event, never spins.
    pub fn join(&mut self) -> RingPhase {
        let Phase::Live(headers) = &*self.phase else { return self.phase() };
        if !joined(headers[0].endpoints.load(JOIN)) {
            return RingPhase::Live;
        }
        let next = match self.take_phase() {
            Phase::Live(headers) => Phase::Retiring(RetireCause::Released, Blocks::Header(headers)),
            other @ (Phase::Building(_)
            | Phase::Ready(_)
            | Phase::Retiring(..)
            | Phase::Terminal(_)) => other,
        };
        *self.phase = next;
        self.phase()
    }

    /// One retirement step, when the owner is Retiring and `steps` has a
    /// unit: the next queued value moved to `sink` (the host's cleanup
    /// custody; a value is never dropped here), else the header freed with
    /// its slot block kept, else the slot block freed: Terminal. An owner
    /// left with no block becomes Terminal without a step.
    pub fn retire_step(&mut self, steps: &mut RingSteps, sink: impl FnOnce(T)) -> RingPhase {
        let Phase::Retiring(cause, blocks) = &mut *self.phase else { return self.phase() };
        let cause = *cause;
        if matches!(blocks, Blocks::Nothing) {
            *self.phase = Phase::Terminal(cause);
            return self.phase();
        }
        if !steps.take() {
            return self.phase();
        }
        match blocks {
            Blocks::Header(headers) => {
                // Each step here reads or frees what an endpoint reached.
                headers[0].tomb.bury();
                if let Some(value) = take_queued(&mut headers[0]) {
                    sink(value);
                    return self.phase();
                }
                let slots = headers.pop().map(|header| header.slots);
                // Replacing the empty header block frees it.
                *blocks = slots.map_or(Blocks::Nothing, Blocks::Slots);
            }
            Blocks::Slots(slots) => {
                // The slot block holds no value now: freeing it drops none.
                drop(mem::take(slots));
                *self.phase = Phase::Terminal(cause);
            }
            Blocks::Nothing => {}
        }
        self.phase()
    }

    /// The joined header's indices (the Loom models: what the join made
    /// visible).
    #[cfg(all(test, loom))]
    fn queued_bounds(&self) -> Option<(usize, usize)> {
        let Phase::Retiring(_, Blocks::Header(headers)) = &*self.phase else { return None };
        let header = &headers[0];
        Some((header.head.load(Ordering::Relaxed), header.tail.load(Ordering::Relaxed)))
    }
}

impl<T> Drop for RetainedRing<T> {
    /// A backing owner dropped before Terminal is a fail-stop (ADR-0157 D3,
    /// class I): a live endpoint may still read the backing and queued
    /// values still need their cleanup, so nothing is freed (the phase is
    /// `ManuallyDrop`) and the drop panics. Dropped in an unwind, it frees
    /// nothing and returns, its storage leaked: forgetting never authorizes
    /// a free, and no second panic starts during an unwind.
    fn drop(&mut self) {
        // Canary `inf_canary_ring_drop_live_silent`: the leak without the
        // fail-stop.
        if matches!(*self.phase, Phase::Terminal(_))
            || std::thread::panicking()
            || cfg!(inf_canary_ring_drop_live_silent)
        {
            return;
        }
        panic!("retained ring dropped before its retirement");
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;

    use super::*;

    /// A ring admitted and built to Ready in its three steps.
    fn ready<T>() -> RetainedRing<T> {
        let mut ring = RetainedRing::admit().expect("within the bound");
        let mut steps = RingSteps::new(3);
        assert_eq!(ring.build_step(&mut steps), RingPhase::Building, "slot block reserved");
        assert_eq!(ring.build_step(&mut steps), RingPhase::Building, "slots initialized");
        assert_eq!(ring.build_step(&mut steps), RingPhase::Ready, "header written");
        assert_eq!(steps.left(), 0, "one unit a step");
        ring
    }

    /// Retires a Retiring ring to Terminal one unit at a time, every value
    /// into `out`; returns the units it took.
    fn retire_all<T>(ring: &mut RetainedRing<T>, out: &mut Vec<T>) -> u32 {
        let mut taken = 0;
        while matches!(ring.phase(), RingPhase::Retiring(_)) {
            let mut steps = RingSteps::new(1);
            ring.retire_step(&mut steps, |value| out.push(value));
            taken += 1 - steps.left();
            assert!(taken <= 64, "a ring retires in bounded steps");
        }
        taken
    }

    /// Publishes from `values` while a permit is free; a value is taken
    /// from the iterator only once its permit exists.
    fn publish<T>(producer: &mut RetainedProducer<T>, values: &mut impl Iterator<Item = T>) {
        let mut reservation = producer.reserve(RETAINED_RING_SLOTS);
        while let Some(permit) = reservation.permit() {
            let Some(value) = values.next() else { break };
            permit.write(value);
        }
    }

    /// Counts drops of the values it tags: a value consumed or retired is
    /// dropped once, by whoever holds it last.
    struct Tracked(u32, Arc<AtomicUsize>);

    impl Drop for Tracked {
        fn drop(&mut self) {
            self.1.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[repr(align(256))]
    #[derive(Debug, PartialEq, Eq)]
    struct Wide([u8; 256]);

    /// The lifecycle on the steps: nothing moves on a zero allowance, the
    /// split is made once, a live ring is neither cancelled nor joined
    /// with an endpoint out, and an empty ring retires in two steps.
    #[test]
    fn a_ring_builds_in_three_steps_splits_once_and_retires_in_two() {
        let mut unbuilt = RetainedRing::<u64>::admit().expect("within the bound");
        assert_eq!(unbuilt.build_step(&mut RingSteps::new(0)), RingPhase::Building);
        assert!(unbuilt.split().is_none(), "a ring being built does not split");
        assert_eq!(unbuilt.cancel(), RingPhase::Retiring(RetireCause::Cancelled));
        assert_eq!(retire_all(&mut unbuilt, &mut Vec::new()), 0, "nothing was built");
        let mut ring = ready::<u64>();
        let (producer, consumer) = ring.split().expect("a ready ring splits");
        assert!(ring.split().is_none(), "the split is made once");
        assert_eq!(ring.cancel(), RingPhase::Live, "a live ring is not cancelled");
        assert_eq!(ring.retire_step(&mut RingSteps::new(1), drop), RingPhase::Live);
        assert_eq!(ring.join(), RingPhase::Live, "both endpoints out");
        drop(producer);
        assert_eq!(ring.join(), RingPhase::Live, "one endpoint out");
        drop(consumer);
        assert_eq!(ring.join(), RingPhase::Retiring(RetireCause::Released));
        let mut zero = RingSteps::new(0);
        assert_eq!(ring.retire_step(&mut zero, drop), RingPhase::Retiring(RetireCause::Released));
        assert_eq!(retire_all(&mut ring, &mut Vec::new()), 2, "the header, then the slots");
        assert_eq!(ring.phase(), RingPhase::Terminal(RetireCause::Released));
    }

    /// The join needs both releases, in either order.
    #[test]
    fn both_drop_orders_join_only_at_the_second_release() {
        for producer_first in [true, false] {
            let mut ring = ready::<u64>();
            let (producer, consumer) = ring.split().expect("ready");
            if producer_first {
                drop(producer);
                assert_eq!(ring.join(), RingPhase::Live, "the consumer is out");
                drop(consumer);
            } else {
                drop(consumer);
                assert_eq!(ring.join(), RingPhase::Live, "the producer is out");
                drop(producer);
            }
            assert_eq!(ring.join(), RingPhase::Retiring(RetireCause::Released));
            retire_all(&mut ring, &mut Vec::new());
        }
    }

    /// FIFO across the wrap through reservations of every size; a full
    /// ring grants nothing, and a full reservation holds no rejected value.
    #[test]
    fn values_move_in_order_across_the_wrap_and_a_full_ring_grants_nothing() {
        let mut ring = ready::<u32>();
        let (mut producer, mut consumer) = ring.split().expect("ready");
        assert_eq!((producer.capacity(), consumer.capacity()), (8, 8));
        let (mut next, mut expected) = (0u32, 0u32);
        for round in 0..40 {
            let batch = round % RETAINED_RING_SLOTS + 1;
            let mut values = next..next + batch as u32;
            publish(&mut producer, &mut values);
            next += batch as u32;
            let got = consumer.consume_batch(RETAINED_RING_SLOTS, |value| {
                assert_eq!(value, expected, "in order, once");
                expected += 1;
            });
            assert_eq!(got, batch);
        }
        let mut reservation = producer.reserve(RETAINED_RING_SLOTS + 1);
        assert_eq!(reservation.granted(), RETAINED_RING_SLOTS, "no more than the slots");
        for value in 0..8 {
            reservation.permit().expect("granted").write(value);
        }
        assert!(reservation.permit().is_none(), "a full reservation has no permit");
        assert_eq!(reservation.publish(), 8);
        let mut full = producer.reserve(1);
        assert_eq!((full.granted(), full.remaining()), (0, 0), "a full ring grants nothing");
        assert!(full.permit().is_none());
        drop(full);
        assert_eq!(consumer.consume_batch(usize::MAX, drop), 8);
        drop((producer, consumer));
        assert_eq!(ring.join(), RingPhase::Retiring(RetireCause::Released));
        retire_all(&mut ring, &mut Vec::new());
    }

    /// A reservation dropped without `publish` still publishes what it
    /// wrote: a written value is never left in an unpublished slot.
    #[test]
    fn a_dropped_reservation_publishes_what_it_wrote() {
        let mut ring = ready::<u32>();
        let (mut producer, mut consumer) = ring.split().expect("ready");
        let mut reservation = producer.reserve(4);
        reservation.permit().expect("room").write(7);
        reservation.permit().expect("room").write(8);
        assert_eq!(reservation.remaining(), 2);
        drop(reservation);
        let mut got = Vec::new();
        assert_eq!(consumer.consume_batch(8, |value| got.push(value)), 2);
        assert_eq!(got, [7, 8]);
        drop((producer, consumer));
        assert_eq!(ring.join(), RingPhase::Retiring(RetireCause::Released));
        retire_all(&mut ring, &mut Vec::new());
    }

    /// Values left queued at the join move to the sink one a step, in
    /// order, each dropped once, by its last holder; the blocks follow.
    #[test]
    fn queued_values_retire_one_a_step_each_dropped_once() {
        let drops = Arc::new(AtomicUsize::new(0));
        let mut ring = ready::<Tracked>();
        let (mut producer, mut consumer) = ring.split().expect("ready");
        let mut values = (0..8).map(|id| Tracked(id, Arc::clone(&drops)));
        publish(&mut producer, &mut values);
        assert_eq!(consumer.consume_batch(3, drop), 3);
        assert_eq!(drops.load(Ordering::Relaxed), 3);
        drop(consumer);
        drop(producer);
        assert_eq!(ring.join(), RingPhase::Retiring(RetireCause::Released));
        let mut out = Vec::new();
        assert_eq!(retire_all(&mut ring, &mut out), 5 + 2, "a value a step, then two blocks");
        assert_eq!(drops.load(Ordering::Relaxed), 3, "a retired value moves, never drops here");
        assert_eq!(out.iter().map(|value| value.0).collect::<Vec<_>>(), [3, 4, 5, 6, 7]);
        drop(out);
        assert_eq!(drops.load(Ordering::Relaxed), 8);
    }

    /// The last release on another thread: the owner's join waits for it,
    /// then retires to Terminal at once, the endpoint threads perhaps still
    /// inside their drops, and every value published is consumed or retired
    /// once. The threads are joined only after the retirement, so nothing
    /// but the join's `Acquire` orders their accesses before the owner's:
    /// under Miri an endpoint access after its release is a data race or a
    /// use after free.
    #[test]
    fn the_owner_retires_at_its_join_while_the_endpoint_threads_finish() {
        let mut ring = ready::<u32>();
        let (mut producer, mut consumer) = ring.split().expect("ready");
        let sender = std::thread::spawn(move || {
            publish(&mut producer, &mut (0..3));
        });
        let receiver = std::thread::spawn(move || {
            let mut got = Vec::new();
            consumer.consume_batch(2, |value| got.push(value));
            got
        });
        let mut waits = 0u64;
        while ring.join() == RingPhase::Live {
            waits += 1;
            std::thread::yield_now();
        }
        let mut retired = Vec::new();
        retire_all(&mut ring, &mut retired);
        assert_eq!(ring.phase(), RingPhase::Terminal(RetireCause::Released));
        sender.join().expect("producer thread");
        let mut values = receiver.join().expect("consumer thread");
        values.append(&mut retired);
        assert_eq!(values, [0, 1, 2], "consumed or retired, once, in order (waits {waits})");
    }

    /// A startup that fails after one endpoint was sent: the host drops the
    /// endpoint it still holds, the owner waits for the sent one's release,
    /// then the values it published retire into the host's custody.
    #[test]
    fn a_failed_startup_transfer_joins_the_sent_endpoint_before_retiring() {
        let mut ring = ready::<u32>();
        let (mut producer, consumer) = ring.split().expect("ready");
        let (go, wait) = mpsc::channel::<()>();
        let sent = std::thread::spawn(move || {
            wait.recv().expect("the host's signal");
            publish(&mut producer, &mut [5, 6].into_iter());
        });
        drop(consumer);
        assert_eq!(ring.join(), RingPhase::Live, "the sent producer is out");
        go.send(()).expect("the thread waits");
        sent.join().expect("producer thread");
        assert_eq!(ring.join(), RingPhase::Retiring(RetireCause::Released));
        let mut out = Vec::new();
        assert_eq!(retire_all(&mut ring, &mut out), 2 + 2);
        assert_eq!(out, [5, 6]);
    }

    /// A cancel at each construction boundary retires exactly what was
    /// built: nothing (no step), the slot block (one), or both (two).
    #[test]
    fn a_cancel_at_each_construction_boundary_retires_what_was_built() {
        for (built, retire_steps) in [(0u32, 0u32), (1, 1), (2, 1), (3, 2)] {
            let mut ring = RetainedRing::<u64>::admit().expect("within the bound");
            for _ in 0..built {
                ring.build_step(&mut RingSteps::new(1));
            }
            assert_eq!(ring.cancel(), RingPhase::Retiring(RetireCause::Cancelled), "{built}");
            assert_eq!(retire_all(&mut ring, &mut Vec::new()), retire_steps, "{built} steps built");
            assert_eq!(ring.phase(), RingPhase::Terminal(RetireCause::Cancelled));
        }
    }

    /// Zero-sized and over-aligned payloads take the same steps and keep
    /// their values and alignment.
    #[test]
    fn zero_sized_and_over_aligned_payloads_take_the_same_steps() {
        let mut ring = ready::<()>();
        let (mut producer, mut consumer) = ring.split().expect("ready");
        publish(&mut producer, &mut core::iter::repeat_n((), 8));
        assert_eq!(consumer.consume_batch(2, drop), 2);
        drop((producer, consumer));
        assert_eq!(ring.join(), RingPhase::Retiring(RetireCause::Released));
        let mut units = Vec::new();
        assert_eq!(retire_all(&mut ring, &mut units), 6 + 2, "zero-sized values still take a step");
        assert_eq!(units.len(), 6);

        let mut ring = ready::<Wide>();
        let (mut producer, mut consumer) = ring.split().expect("ready");
        publish(&mut producer, &mut (0..8u8).map(|byte| Wide([byte; 256])));
        consumer.consume_batch(1, |value| {
            assert_eq!(value, Wide([0; 256]));
            assert_eq!(core::ptr::from_ref(&value).addr() % 256, 0, "aligned");
        });
        drop((producer, consumer));
        assert_eq!(ring.join(), RingPhase::Retiring(RetireCause::Released));
        let mut wide = Vec::new();
        assert_eq!(retire_all(&mut ring, &mut wide), 7 + 2);
        assert_eq!(wide.iter().map(|value| value.0[0]).collect::<Vec<_>>(), [1, 2, 3, 4, 5, 6, 7]);
    }

    /// The byte bound: 65,536 admitted, one byte over refused; a payload
    /// whose slot block is over is refused at admission, with its size.
    #[test]
    fn a_block_over_the_byte_bound_is_refused_at_admission() {
        assert_eq!(within(RingBlock::Slots, RETAINED_RING_BACKING_BYTES_MAX), Ok(65_536));
        let over = within(RingBlock::Header, RETAINED_RING_BACKING_BYTES_MAX + 1);
        assert_eq!(over, Err(RingLayoutRefused { block: RingBlock::Header, bytes: Some(65_537) }));
        let at = RetainedRing::<[u8; 8_192]>::admit().expect("8 × 8,192 = 65,536");
        let header = size_of::<Header<[u8; 8_192]>>();
        assert_eq!(at.requested_bytes(), 65_536 + header);
        let refused = RetainedRing::<[u8; 8_193]>::admit().map(drop).expect_err("8 × 8,193");
        assert_eq!(refused, RingLayoutRefused { block: RingBlock::Slots, bytes: Some(65_544) });
        assert_eq!(refused.to_string(), "retained ring slot block of 65544 bytes is over 65536",);
        let mut at = at;
        at.cancel();
        retire_all(&mut at, &mut Vec::new());
    }

    /// Canary `inf_canary_ring_drop_live_silent` (KR5): a backing owner
    /// dropped before Terminal fails stop. Admitted and unbuilt, it holds
    /// no block, so the test leaks nothing.
    #[test]
    #[should_panic(expected = "retained ring dropped before its retirement")]
    fn a_backing_dropped_before_its_retirement_fails_stop() {
        drop(RetainedRing::<u64>::admit().expect("within the bound"));
    }

    /// A backing dropped in a caught unwind frees nothing and starts no
    /// second panic: the test's process is not aborted, and the endpoints
    /// the unwind drops after it still release on its header.
    #[test]
    #[cfg_attr(miri, ignore = "leaks the dropped backing's blocks by design")]
    fn a_backing_dropped_in_an_unwind_frees_nothing_and_does_not_abort() {
        let caught = std::panic::catch_unwind(|| {
            let mut ring = ready::<u64>();
            let (mut producer, consumer) = ring.split().expect("ready");
            publish(&mut producer, &mut (0..3));
            let _held = (ring, producer, consumer);
            panic!("the host's own failure");
        });
        assert!(caught.is_err(), "the host's panic is caught, not an abort");
    }

    /// A live backing dropped fails stop and frees nothing: the endpoints
    /// the unwind drops after it still release on its header.
    #[test]
    #[cfg_attr(miri, ignore = "leaks the dropped backing's blocks by design")]
    #[should_panic(expected = "retained ring dropped before its retirement")]
    fn a_live_backing_dropped_fails_stop_and_frees_nothing() {
        let mut ring = ready::<u64>();
        let (mut producer, consumer) = ring.split().expect("ready");
        publish(&mut producer, &mut (0..3));
        drop(ring);
        drop((producer, consumer));
    }
}

#[cfg(all(test, loom))]
mod loom_tests {
    use std::sync::atomic::{AtomicUsize as CensusCount, Ordering as CensusOrder};

    use loom::thread;

    use super::*;

    /// ADR-0157 D5's three programs: both endpoints released concurrently,
    /// then each held by the fixture while the other releases; and the
    /// concurrent release again with the owner retiring to Terminal as soon
    /// as its join reads zero, before the fixture's thread join.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Case {
        Concurrent,
        ProducerHeld,
        ConsumerHeld,
        RetireBeforeThreadJoin,
    }

    impl Case {
        /// Whether the fixture holds an endpoint at the owner's first join.
        fn holds_an_endpoint(self) -> bool {
            match self {
                Case::ProducerHeld | Case::ConsumerHeld => true,
                Case::Concurrent | Case::RetireBeforeThreadJoin => false,
            }
        }
    }

    /// What the explored executions observed before the fixture's thread
    /// joins; plain atomics outside the model, which steer nothing in it.
    #[derive(Default)]
    struct Census {
        completed: CensusCount,
        prior_joined: CensusCount,
        prior_waiting: CensusCount,
    }

    fn ready() -> RetainedRing<u32> {
        let mut ring = RetainedRing::admit().expect("within the bound");
        let mut steps = RingSteps::new(3);
        for _ in 0..3 {
            ring.build_step(&mut steps);
        }
        assert_eq!(ring.phase(), RingPhase::Ready);
        ring
    }

    fn publish_two(mut producer: RetainedProducer<u32>) {
        let mut reservation = producer.reserve(2);
        for value in [1, 2] {
            reservation.permit().expect("an empty ring has room").write(value);
        }
        drop(reservation);
        drop(producer);
    }

    fn consume_one(mut consumer: RetainedConsumer<u32>) -> Vec<u32> {
        let mut got = Vec::new();
        consumer.consume_batch(1, |value| got.push(value));
        drop(consumer);
        got
    }

    /// The owner's join attempt before any fixture thread is joined; when
    /// it joins, the indices it made visible.
    fn observe(
        ring: &mut RetainedRing<u32>,
        case: Case,
        census: &Census,
    ) -> Option<(usize, usize)> {
        match ring.join() {
            RingPhase::Live => {
                census.prior_waiting.fetch_add(1, CensusOrder::Relaxed);
                None
            }
            RingPhase::Retiring(RetireCause::Released) => {
                assert!(
                    !case.holds_an_endpoint(),
                    "JOIN_WITH_HELD_ENDPOINT: the owner joined while the fixture holds an endpoint"
                );
                census.prior_joined.fetch_add(1, CensusOrder::Relaxed);
                ring.queued_bounds()
            }
            other => panic!("a live ring's join reads Live or Retiring, not {other:?}"),
        }
    }

    /// Every endpoint released and every fixture thread joined: the final
    /// join, the retirement, and every published value consumed or retired
    /// once, in order.
    fn finish(mut ring: RetainedRing<u32>, consumed: Vec<u32>, published: &[u32]) {
        if ring.phase() == RingPhase::Live {
            let joined = ring.join();
            assert_eq!(joined, RingPhase::Retiring(RetireCause::Released), "FINAL_JOIN_MISSING");
        }
        let mut values = consumed;
        retire(&mut ring, &mut values);
        assert_eq!(ring.phase(), RingPhase::Terminal(RetireCause::Released));
        assert_eq!(values, published, "every value consumed or retired once, in order");
    }

    /// Retires a joined ring within one allowance, each value into `values`.
    fn retire(ring: &mut RetainedRing<u32>, values: &mut Vec<u32>) {
        let mut steps = RingSteps::new(16);
        while matches!(ring.phase(), RingPhase::Retiring(_)) && steps.left() > 0 {
            ring.retire_step(&mut steps, |value| values.push(value));
        }
    }

    fn program(case: Case, census: &Census) {
        let mut ring = ready();
        let (producer, consumer) = ring.split().expect("a ready ring splits");
        match case {
            Case::Concurrent => {
                let sender = thread::spawn(move || publish_two(producer));
                let receiver = thread::spawn(move || consume_one(consumer));
                let seen = observe(&mut ring, case, census);
                if let Some((_, tail)) = seen {
                    assert_eq!(tail, 2, "JOIN_VISIBILITY_GAP: the producer's publication");
                }
                sender.join().expect("producer");
                let consumed = receiver.join().expect("consumer");
                if let Some((head, _)) = seen {
                    assert_eq!(head, consumed.len(), "JOIN_VISIBILITY_GAP: the consumer's index");
                }
                finish(ring, consumed, &[1, 2]);
            }
            Case::ProducerHeld => {
                let receiver = thread::spawn(move || consume_one(consumer));
                observe(&mut ring, case, census);
                let consumed = receiver.join().expect("consumer");
                drop(producer);
                finish(ring, consumed, &[]);
            }
            Case::ConsumerHeld => {
                let sender = thread::spawn(move || publish_two(producer));
                observe(&mut ring, case, census);
                sender.join().expect("producer");
                drop(consumer);
                finish(ring, Vec::new(), &[1, 2]);
            }
            Case::RetireBeforeThreadJoin => {
                let sender = thread::spawn(move || publish_two(producer));
                let receiver = thread::spawn(move || consume_one(consumer));
                let mut retired = Vec::new();
                if observe(&mut ring, case, census).is_some() {
                    retire(&mut ring, &mut retired);
                    assert_eq!(ring.phase(), RingPhase::Terminal(RetireCause::Released));
                }
                sender.join().expect("producer");
                let mut values = receiver.join().expect("consumer");
                values.append(&mut retired);
                finish(ring, values, &[1, 2]);
            }
        }
        census.completed.fetch_add(1, CensusOrder::Relaxed);
    }

    /// Explores `case` whole: no preemption, permutation or time cutoff.
    fn explore(case: Case) -> (usize, usize, usize) {
        let census = std::sync::Arc::new(Census::default());
        let mut builder = loom::model::Builder::new();
        builder.preemption_bound = None;
        builder.max_permutations = None;
        builder.max_duration = None;
        let model = std::sync::Arc::clone(&census);
        builder.check(move || program(case, &model));
        let completed = census.completed.load(CensusOrder::Relaxed);
        let joined = census.prior_joined.load(CensusOrder::Relaxed);
        let waiting = census.prior_waiting.load(CensusOrder::Relaxed);
        assert!(completed > 0, "INCOMPLETE_DOMAIN: {case:?}");
        assert_eq!(joined + waiting, completed, "one observation an execution");
        println!("MODEL {case:?} executions={completed} joined={joined} waiting={waiting}");
        (completed, joined, waiting)
    }

    /// Both endpoints released concurrently: the owner's join before the
    /// fixture's thread joins sees both outcomes across the executions, and
    /// whenever it joins, it sees both endpoints' last writes.
    #[test]
    fn loom_retained_join_sees_both_endpoints_final_writes() {
        let (completed, joined, waiting) = explore(Case::Concurrent);
        assert!(joined > 0 && waiting > 0, "PRE_JOIN_BRANCH_MISSING: {completed} {joined}");
    }

    /// The producer held: no execution joins before the fixture releases it.
    #[test]
    fn loom_retained_join_waits_while_the_producer_is_held() {
        let (_, joined, _) = explore(Case::ProducerHeld);
        assert_eq!(joined, 0, "JOIN_WITH_HELD_ENDPOINT");
    }

    /// The consumer held: no execution joins before the fixture releases it.
    #[test]
    fn loom_retained_join_waits_while_the_consumer_is_held() {
        let (_, joined, _) = explore(Case::ConsumerHeld);
        assert_eq!(joined, 0, "JOIN_WITH_HELD_ENDPOINT");
    }

    /// Both endpoints released concurrently, the owner retiring to Terminal
    /// the moment its join reads zero, before any fixture thread is joined:
    /// only the join's `Acquire` orders the endpoints' accesses before the
    /// retirement's, and an endpoint access after its release, to a slot or
    /// to the header's own fields (the header's tomb), is a causality
    /// violation. Both outcomes of that first join are explored.
    #[test]
    fn loom_retained_retirement_before_the_thread_join() {
        let (completed, joined, waiting) = explore(Case::RetireBeforeThreadJoin);
        assert!(joined > 0 && waiting > 0, "PRE_JOIN_BRANCH_MISSING: {completed} {joined}");
    }
}
