//! The cell executor (ADR-0003): single-threaded and `!Send`, with a fast path that costs
//! (nearly) nothing — a future completing on its first poll never allocates a task slot and
//! never touches `malloc` (its state machine is placed into a reusable scratch buffer).
//!
//! Suspension is the slow path by design: fabric hops (M0), blocking ops (M1), fsync gating (M2)
//! and cold-tier reads (M7) all promote the future into a slab slot and park it on a typed gate
//! (see [`crate::gate`]).
//!
//! # A standard `Waker` at a wake bridge (ADR-0003 A1)
//!
//! A `Waker` is `Send + Sync`, so its four operations must be sound on any thread, whatever the
//! future that cloned it. A waker's data is its task's **wake header**: first the
//! [`WakeBridge`], atomics only, which any thread may touch; then the owner part (the task's
//! state and slot, the executor's shared state, the owner lane's count `o` and the held list's
//! links), which only the thread that built the executor reaches. Each operation compares the
//! bridge's mark with the calling thread's ([`owner_part`]):
//!
//! - **the owner lane** (the marks match): HEAD's cell-local transition, with no atomic
//!   read-modify-write, fence or ordered access (ADR-0003 A1.5); `o` is compared with
//!   `LANE_COUNT_MAX` before it grows;
//! - **the foreign lane** (any other thread, and every thread once the executor is gone):
//!   [`foreign`], five `#[inline(never)]` functions over `inf_foundation::wake`. A foreign wake
//!   is queued on the executor's remote stack and delivered by the next [`CellExecutor::run_ready`]
//!   (its drain); [`CellExecutor::has_ready`] sees it first.
//!
//! A finished task's header is decided only after its future's drop: kept spare, freed, or
//! **Held** — linked into a list in the shared state — while a waker names it or a foreign wake
//! for it is queued. The executor's drop closes every header it still holds: the last waker of a
//! closed header frees it, on whichever thread drops it, and the stack with the last. No product
//! path moves a waker off its cell (ADR-0149 D1); the foreign lane makes a violation a counted,
//! sound wake (`waker_foreign_ops`). `SAFETY.md` holds the audit of this module's `unsafe`.

use core::alloc::Layout;
use core::cell::{Cell, RefCell};
use core::future::Future;
use core::marker::PhantomData;
use core::mem::ManuallyDrop;
use core::pin::Pin;
use core::ptr::NonNull;
use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
use std::alloc::{alloc, dealloc, handle_alloc_error};
#[allow(clippy::disallowed_types, reason = "container: E")]
use std::collections::VecDeque;
use std::rc::Rc;

use inf_foundation::limits::LANE_COUNT_MAX;
use inf_foundation::wake::{Closing, Delivery, RemoteStack, Settled, ThreadMark, WakeBridge};

/// Slot value while a task header is not (yet) bound to a slab slot.
const UNASSIGNED: u32 = u32::MAX;

// ---- Planted violations (canaries; each a `cfg`, never a feature) -------------------------

/// The lane compare answers the owner part on every thread.
const PLANT_OWNER_UNCHECKED: bool = cfg!(inf_canary_waker_owner_unchecked);
/// A finished header's reuse reads `o` and not the lane word.
const PLANT_REUSE_OWNER_COUNT_ONLY: bool = cfg!(inf_canary_wake_reuse_owner_count_only);
/// The swap that leaves a closed header at `c = 0` with `Q` clear frees nothing.
const PLANT_CLOSED_HEADER_KEPT: bool = cfg!(inf_canary_closed_header_kept);
/// The owner lane compares no count with `LANE_COUNT_MAX` (the foreign lane's half is
/// `inf_foundation::wake`'s).
const PLANT_COUNT_UNBOUNDED: bool = cfg!(inf_canary_wake_count_unbounded);
/// An owner-lane drop settles a Dead header too, not only a Held one.
const PLANT_SETTLE_ON_DEAD: bool = cfg!(inf_canary_settle_on_dead);
/// An owner-lane drop of a Held header's last waker does not settle it.
const PLANT_SETTLE_HELD_SKIPPED: bool = cfg!(inf_canary_settle_held_skipped);
/// `close` does not drop the owner part, so its `Rc` keeps the shared state.
const PLANT_CLOSE_KEEPS_OWNER_PART: bool = cfg!(inf_canary_close_keeps_owner_part);

/// Identifies a spawned task while it is live. Slot indices are reused;
/// `generation` makes stale ids detectable (`CellExecutor::is_live`).
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct TaskId {
    slot: u32,
    generation: u64,
}

/// Outcome of [`CellExecutor::poll_immediate`].
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum PollImmediate {
    /// The future finished on its first poll — the fast path. No task slot
    /// was allocated, no waker survives.
    Completed,
    /// The future suspended and was promoted to a task slot; it resumes via
    /// its waker (typed gates) and [`CellExecutor::run_ready`].
    Suspended(TaskId),
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum TaskState {
    /// Suspended, waiting for a wake.
    Idle,
    /// In the ready queue.
    Queued,
    /// Being polled right now.
    Running,
    /// Woken while being polled — requeue on `Pending`.
    RunningWoken,
    /// Finished or spare; wakes are no-ops; not yet decided, or kept spare.
    Dead,
    /// Finished, and a waker still names the header or a foreign wake for it is queued: kept on
    /// the held list until its last waker's drop or the drain frees it. Wakes are no-ops.
    Held,
}

// ---- The wake header ---------------------------------------------------------------------------

/// The executor's shared state, one `Rc` allocation that the executor and every live header's
/// owner part hold: a waker reaches it through its header, never through the executor, whose
/// `&mut self` during a poll covers none of it (ADR-0003 A1.2.2).
#[allow(clippy::disallowed_types, reason = "container: E")]
struct ExecShared {
    /// The ready queue: `RefCell`, single-threaded interior mutability, no atomics (L1).
    ready: RefCell<VecDeque<u32>>,
    /// The held list's head.
    held: Cell<Option<Header>>,
}

/// A task's wake header: the bridge first, the owner part after it (`repr(C)`), so a waker's data
/// pointer is the bridge's address too.
#[repr(C)]
struct WakeHeader {
    bridge: WakeBridge,
    /// Dropped in place by the executor's drop when it closes the header; a closed header is the
    /// bridge alone.
    owner: ManuallyDrop<OwnerPart>,
}

/// What only the owner thread touches, reached through [`owner_part`]'s compare.
struct OwnerPart {
    state: Cell<TaskState>,
    slot: Cell<u32>,
    shared: Rc<ExecShared>,
    /// `o`, the wakers the owner lane counted: the header's live wakers are `o + c`.
    owner_count: Cell<i64>,
    held_prev: Cell<Option<Header>>,
    held_next: Cell<Option<Header>>,
}

/// A wake header's address, with the provenance of its whole allocation.
type Header = NonNull<WakeHeader>;

// The bridge's four words, the state and slot, the shared state's `Rc`, `o` and the held list's
// two links: nine words, 40 bytes over a header of HEAD's `Rc` waker (ADR-0003 A1.2.2).
const _: () = assert!(size_of::<WakeHeader>() == 72, "a wake header is nine words");

/// The owner part of a header.
///
/// # Safety
/// `header` is live and not closed, and the caller is the owner thread.
unsafe fn owner<'a>(header: Header) -> &'a OwnerPart {
    // SAFETY: per the caller's contract; the owner part is dropped only by `close`, after which
    // no owner-thread path reaches it (its mark is `MARK_CLOSED`).
    unsafe { &(*header.as_ptr()).owner }
}

/// The bridge of a header.
///
/// # Safety
/// `header` is live: a waker names it, a push keeps it, or the executor holds it.
unsafe fn bridge<'a>(header: Header) -> &'a WakeBridge {
    // SAFETY: per the caller's contract; the bridge is atomics only, so any thread may read it.
    unsafe { &(*header.as_ptr()).bridge }
}

/// The lane compare (ADR-0003 A1.2.2): `data`'s header if the calling thread is the one whose
/// mark it carries; `None` on any other thread, and on every thread once the header is closed
/// (its mark is `MARK_CLOSED`, no thread's).
///
/// # Safety
/// `data` is a live header's address, named by the waker the caller holds.
#[inline(always)]
unsafe fn owner_part(data: *const ()) -> Option<Header> {
    // SAFETY: the caller's waker keeps the header allocated; the bridge, at offset 0, is atomics
    // only, so any thread may form this reference.
    let marked = unsafe { &*data.cast::<WakeBridge>() }.owner_mark();
    let owned = marked == ThreadMark::current() || PLANT_OWNER_UNCHECKED;
    // SAFETY: `data` is a header's own pointer (`waker_ref`, `waker_clone`), never null.
    owned.then(|| unsafe { NonNull::new_unchecked(data.cast_mut().cast::<WakeHeader>()) })
}

/// The owner lane's wake transition, HEAD's: Idle → Queued and pushed; Running → RunningWoken;
/// otherwise nothing. Runs inside `Waker::wake`, so it touches only the header and the shared
/// ready queue, never the executor.
fn wake_task(owner: &OwnerPart) {
    match owner.state.get() {
        TaskState::Idle => {
            let slot = owner.slot.get();
            debug_assert_ne!(slot, UNASSIGNED, "idle task without a slot");
            owner.state.set(TaskState::Queued);
            owner.shared.ready.borrow_mut().push_back(slot);
        }
        // Woken during its own poll (wake-before-suspend): remember it so
        // the executor requeues on `Pending` — the classic lost-wakeup hole.
        TaskState::Running => owner.state.set(TaskState::RunningWoken),
        // Already queued / already flagged / finished: wakes coalesce.
        TaskState::Queued | TaskState::RunningWoken | TaskState::Dead | TaskState::Held => {}
    }
}

/// The owner lane's clone: `o + 1`, compared with the bound first (ADR-0003 A1.2.6).
#[inline(always)]
fn owner_clone(owner: &OwnerPart) {
    let count = owner.owner_count.get();
    if !PLANT_COUNT_UNBOUNDED && count >= LANE_COUNT_MAX {
        // Fail-stop before the count is written, as `Rc` and `Arc` at their bounds.
        std::process::abort();
    }
    owner.owner_count.set(count + 1);
}

/// The owner lane's drop: `o − 1`, and on a Held header its settle, which frees the header when
/// no waker names it and it is not queued (ADR-0003 A1.2.4).
///
/// # Safety
/// `header` came from the lane compare; nothing touches it after this call, which may free it.
#[inline(always)]
unsafe fn owner_drop(header: Header) {
    // SAFETY: the compare matched, so this is the owner thread and the header is open.
    let owner = unsafe { owner(header) };
    owner.owner_count.set(owner.owner_count.get() - 1);
    let settles = match owner.state.get() {
        TaskState::Held => true,
        TaskState::Dead => PLANT_SETTLE_ON_DEAD,
        TaskState::Idle | TaskState::Queued | TaskState::Running | TaskState::RunningWoken => false,
    };
    if settles && !PLANT_SETTLE_HELD_SKIPPED {
        // SAFETY: as above; `owner` is not used after this call.
        unsafe { foreign::settle_held(header) };
    }
}

// ---- The waker vtable: the lane compare, then the owner lane or `foreign` ------------------

/// # Safety
/// `data` is a live header's address: the waker being cloned names it.
unsafe fn waker_clone(data: *const ()) -> RawWaker {
    // SAFETY: per this function's contract.
    match unsafe { owner_part(data) } {
        // SAFETY: the compare matched: the owner thread, an open header.
        Some(header) => owner_clone(unsafe { owner(header) }),
        // SAFETY: per this function's contract.
        None => unsafe { foreign::clone(data) },
    }
    RawWaker::new(data, &WAKER_VTABLE)
}

/// # Safety
/// `data` is a live header's address, which this waker names; the call consumes the waker.
unsafe fn waker_wake(data: *const ()) {
    // SAFETY: per this function's contract.
    match unsafe { owner_part(data) } {
        Some(header) => {
            // SAFETY: the compare matched: the owner thread, an open header.
            wake_task(unsafe { owner(header) });
            // SAFETY: as above; the waker is consumed here.
            unsafe { owner_drop(header) };
        }
        // SAFETY: per this function's contract.
        None => unsafe { foreign::wake(data) },
    }
}

/// # Safety
/// `data` is a live header's address, which this waker names; nothing is consumed.
unsafe fn waker_wake_by_ref(data: *const ()) {
    // SAFETY: per this function's contract.
    match unsafe { owner_part(data) } {
        // SAFETY: the compare matched: the owner thread, an open header.
        Some(header) => wake_task(unsafe { owner(header) }),
        // SAFETY: per this function's contract.
        None => unsafe { foreign::wake_by_ref(data) },
    }
}

/// # Safety
/// `data` is a live header's address, which this waker names; its count is released once.
unsafe fn waker_drop(data: *const ()) {
    // SAFETY: per this function's contract.
    match unsafe { owner_part(data) } {
        // SAFETY: the compare matched; the waker is consumed here.
        Some(header) => unsafe { owner_drop(header) },
        // SAFETY: per this function's contract.
        None => unsafe { foreign::drop(data) },
    }
}

/// The workspace's one waker vtable (ADR-0003 A1.6's source census holds it here).
static WAKER_VTABLE: RawWakerVTable =
    RawWakerVTable::new(waker_clone, waker_wake, waker_wake_by_ref, waker_drop);

/// A `Waker` view over a header the executor holds, with no count: the executor polls with this;
/// the future pays a count only if it clones the waker (it is really suspending).
fn waker_ref(header: Header) -> ManuallyDrop<Waker> {
    let raw = RawWaker::new(header.as_ptr().cast_const().cast::<()>(), &WAKER_VTABLE);
    // SAFETY: the vtable's four functions are sound on any thread (the lane compare, then the
    // owner lane or `foreign`); `ManuallyDrop` keeps this borrowed view from releasing a count it
    // never took. The header outlives the poll: the executor holds it.
    ManuallyDrop::new(unsafe { Waker::from_raw(raw) })
}

/// The foreign lane (ADR-0003 A1.2.5, A1.6): the four entries the vtable's arms call when the
/// lane compare fails, and `settle_held`, the owner lane's free of a Held header. Each is
/// `#[inline(never)]`, so its symbol bounds what the waker gate scans as the owner lane: these
/// five bodies are its boundary, and every atomic of the waker path is in them or in
/// `inf_foundation::wake`, which they call. `executor::foreign` passes the bridge only its own
/// remote stack (`WakeBridge::stack`) and, as its push's node, the header's own address.
mod foreign {
    use core::ptr::NonNull;

    use inf_foundation::wake::{Freed, Released, RemoteStack, WakeBridge};

    use super::{Header, WakeHeader};

    /// The bridge at `data` and the remote stack it queues on.
    ///
    /// # Safety
    /// `data` is a live header's address, which the caller's waker names: it keeps the header
    /// and, through the stack's count of a closed header, the stack allocated (ADR-0003 A1.3).
    #[inline(always)]
    unsafe fn bridge_and_stack<'a>(data: *const ()) -> (&'a WakeBridge, &'a RemoteStack) {
        // SAFETY: per the caller's contract; both are atomics only, `Sync` by their fields.
        unsafe {
            let bridge = &*data.cast::<WakeBridge>();
            (bridge, &*bridge.stack())
        }
    }

    /// The node a push links: the header's own address, whose provenance reaches the whole
    /// header, which the drain needs (a reference to the bridge reaches its 32 bytes alone).
    ///
    /// # Safety
    /// `data` is a live header's address.
    #[inline(always)]
    unsafe fn node(data: *const ()) -> NonNull<WakeBridge> {
        // SAFETY: a header's address is never null.
        unsafe { NonNull::new_unchecked(data.cast_mut().cast::<WakeBridge>()) }
    }

    /// A foreign clone: `c + 1` under the bound's compare.
    ///
    /// # Safety
    /// `data` is a live header's address, which the caller's waker names.
    #[inline(never)]
    pub(super) unsafe fn clone(data: *const ()) {
        // SAFETY: per this function's contract.
        let (bridge, stack) = unsafe { bridge_and_stack(data) };
        bridge.foreign_clone(stack);
    }

    /// A foreign `wake_by_ref`: queued for the drain unless the header is closed.
    ///
    /// # Safety
    /// `data` is a live header's address, which the caller's waker names.
    #[inline(never)]
    pub(super) unsafe fn wake_by_ref(data: *const ()) {
        // SAFETY: per this function's contract.
        let (bridge, stack) = unsafe { bridge_and_stack(data) };
        // SAFETY: per this function's contract.
        let _ = bridge.foreign_wake_by_ref_at(stack, unsafe { node(data) });
    }

    /// A foreign `wake`, which consumes its waker; frees a closed header it leaves unnamed.
    ///
    /// # Safety
    /// `data` is a live header's address, which the caller's waker names and gives up here.
    #[inline(never)]
    pub(super) unsafe fn wake(data: *const ()) {
        // SAFETY: per this function's contract.
        let (bridge, stack) = unsafe { bridge_and_stack(data) };
        // SAFETY: per this function's contract.
        let released = bridge.foreign_wake_at(stack, unsafe { node(data) });
        // SAFETY: `Last` makes this call the header's freer; nothing above touches it again.
        unsafe { settle_released(data, released) };
    }

    /// A foreign drop; frees a closed header it leaves unnamed.
    ///
    /// # Safety
    /// `data` is a live header's address, which the caller's waker names and gives up here.
    #[inline(never)]
    pub(super) unsafe fn drop(data: *const ()) {
        // SAFETY: per this function's contract.
        let (bridge, stack) = unsafe { bridge_and_stack(data) };
        // SAFETY: per this function's contract.
        let released = bridge.foreign_drop_at(stack, unsafe { node(data) });
        // SAFETY: `Last` makes this call the header's freer; nothing above touches it again.
        unsafe { settle_released(data, released) };
    }

    /// # Safety
    /// `released` is the header's own swap's answer; on `Last` this thread is its one freer.
    #[inline(always)]
    unsafe fn settle_released(data: *const (), released: Released) {
        match released {
            // SAFETY: per this function's contract: a closed header no waker names, whose
            // address is never null.
            Released::Last => unsafe {
                super::free_closed(NonNull::new_unchecked(data.cast_mut().cast::<WakeHeader>()));
            },
            Released::Queued | Released::Kept => {}
        }
    }

    /// The owner lane's settle of a Held header (ADR-0003 A1.3): one `Relaxed` load of the lane
    /// word, and the header unlinked and freed when no waker names it and it is not queued.
    /// Runs inside a waker's drop, during another task's poll or none, so it reaches the held
    /// list through the header's own `Rc` of the shared state, never through the executor.
    ///
    /// # Safety
    /// `header` is an open header on the owner thread whose waker was just dropped; nothing
    /// touches it after this call, which may free it.
    #[inline(never)]
    pub(super) unsafe fn settle_held(header: Header) {
        // SAFETY: per this function's contract.
        let owner_count = unsafe { super::owner(header) }.owner_count.get();
        // SAFETY: as above.
        if !unsafe { super::bridge(header) }.quiet(owner_count) {
            return;
        }
        // SAFETY: an open header on the owner thread, unnamed and unqueued.
        unsafe {
            super::unlink_held(header);
            super::free_open(header);
        }
    }

    /// Releases one count of `stack`, and frees it when that count was the last.
    ///
    /// # Safety
    /// `stack` is an executor's remote stack and the caller holds one of its counts.
    pub(super) unsafe fn release_stack(stack: NonNull<RemoteStack>) {
        // SAFETY: the caller's count keeps the stack allocated across the release.
        match unsafe { stack.as_ref() }.release() {
            // SAFETY: the count reached zero: no header and no executor names the stack.
            Freed::Stack => core::mem::drop(unsafe { Box::from_raw(stack.as_ptr()) }),
            Freed::No => {}
        }
    }
}

/// Unlinks a Held header from the held list in O(1), through its own `Rc` of the shared state
/// and its neighbours' links; a header in any other state is on no list.
///
/// # Safety
/// `header` is open and the caller is the owner thread.
unsafe fn unlink_held(header: Header) {
    // SAFETY: per this function's contract.
    let owner = unsafe { owner(header) };
    if owner.state.get() != TaskState::Held {
        return;
    }
    let (prev, next) = (owner.held_prev.get(), owner.held_next.get());
    match prev {
        // SAFETY: a list neighbour is open and Held: the list holds it.
        Some(prev) => unsafe { self::owner(prev) }.held_next.set(next),
        None => owner.shared.held.set(next),
    }
    if let Some(next) = next {
        // SAFETY: as above.
        unsafe { self::owner(next) }.held_prev.set(prev);
    }
    owner.held_prev.set(None);
    owner.held_next.set(None);
}

/// Links a finished header at the held list's head: a waker names it or a push holds it.
///
/// # Safety
/// `header` is open, finished and on no list; the caller is the owner thread.
unsafe fn link_held(header: Header) {
    // SAFETY: per this function's contract.
    let owner = unsafe { owner(header) };
    debug_assert_eq!(owner.state.get(), TaskState::Dead, "a header is decided once");
    owner.state.set(TaskState::Held);
    let next = owner.shared.held.replace(Some(header));
    owner.held_prev.set(None);
    owner.held_next.set(next);
    if let Some(next) = next {
        // SAFETY: a list neighbour is open and Held: the list holds it.
        unsafe { self::owner(next) }.held_prev.set(Some(header));
    }
}

/// Frees an open header: its owner part (and with it its `Rc` of the shared state), then its
/// allocation.
///
/// # Safety
/// `header` is open, no waker names it, no push holds it and no list links it; the caller is
/// the owner thread, which allocated it.
unsafe fn free_open(header: Header) {
    // SAFETY: per this function's contract: nothing else reaches the header, so these are the
    // last accesses; the owner part is dropped once, here, and the box frees the rest.
    unsafe {
        ManuallyDrop::drop(&mut (*header.as_ptr()).owner);
        drop(Box::from_raw(header.as_ptr()));
    }
}

/// Frees a closed header, then releases the remote stack's count of it, which frees the stack
/// when it was the last (ADR-0003 A1.3).
///
/// # Safety
/// `header` is closed and this thread's swap (or the executor's close or last take) left it at
/// `c = 0` with `Q` clear: no waker names it, no push holds it, and this thread is its one freer.
unsafe fn free_closed(header: Header) {
    if PLANT_CLOSED_HEADER_KEPT {
        return;
    }
    // SAFETY: per this function's contract; the stack outlives every header it counts.
    let stack = unsafe { bridge(header) }.stack();
    // SAFETY: as above; the owner part was dropped in place at the close, and `ManuallyDrop`
    // keeps the box from dropping it again.
    drop(unsafe { Box::from_raw(header.as_ptr()) });
    // SAFETY: the stack's count of this header is the caller's to release; the bridge's stack
    // address is never null.
    unsafe { foreign::release_stack(NonNull::new_unchecked(stack.cast_mut())) };
}

/// Whether a finished header is unnamed and unqueued: `o + c = 0` with `Q` clear, one `Relaxed`
/// load of the lane word (ADR-0003 A1.7 says why it needs no order).
///
/// # Safety
/// `header` is open and the caller is the owner thread.
unsafe fn quiet(header: Header) -> bool {
    // SAFETY: per this function's contract.
    let owner_count = unsafe { owner(header) }.owner_count.get();
    if PLANT_REUSE_OWNER_COUNT_ONLY {
        return owner_count == 0;
    }
    // SAFETY: as above.
    unsafe { bridge(header) }.quiet(owner_count)
}

// ---- Type-erased task storage ---------------------------------------------

/// Owned, type-erased future storage. The future is moved in **before** its
/// first poll and never moves again — the slab stores this struct (plain
/// data), not the future, so Pin's no-move contract holds structurally.
struct RawFut {
    /// Start of the buffer; the future lives at offset 0.
    ptr: NonNull<u8>,
    /// The ALLOCATION layout (may exceed the future's own layout when the
    /// buffer came from the reusable scratch slot). Dealloc uses this.
    alloc_layout: Layout,
    poll_fn: unsafe fn(*mut u8, &mut Context<'_>) -> Poll<()>,
    drop_fn: unsafe fn(*mut u8),
}

impl RawFut {
    /// # Safety
    /// `ptr` must hold a live, initialized `F` and `alloc_layout` must be
    /// the layout the buffer was allocated with (size 0 ⇒ dangling, never
    /// freed).
    unsafe fn poll(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        // SAFETY: per constructor contract, `ptr` holds a live `F` that has
        // not moved since its first poll.
        unsafe { (self.poll_fn)(self.ptr.as_ptr(), cx) }
    }
}

impl Drop for RawFut {
    fn drop(&mut self) {
        // SAFETY: the future is live (RawFut is dropped exactly once, and
        // only while it owns the future); the buffer was allocated with
        // `alloc_layout` unless zero-sized.
        unsafe {
            (self.drop_fn)(self.ptr.as_ptr());
            if self.alloc_layout.size() > 0 {
                dealloc(self.ptr.as_ptr(), self.alloc_layout);
            }
        }
    }
}

/// # Safety
/// `ptr` points at a live `F` placed by `spawn`, pinned in place since before its first poll.
unsafe fn poll_shim<F: Future<Output = ()>>(ptr: *mut u8, cx: &mut Context<'_>) -> Poll<()> {
    // SAFETY: caller contract — `ptr` holds a live `F`, pinned in place
    // since before its first poll.
    let fut = unsafe { &mut *ptr.cast::<F>() };
    // SAFETY: the storage never moves (RawFut owns it; slab stores RawFut by
    // value but the buffer it points to is heap-stable).
    unsafe { Pin::new_unchecked(fut) }.poll(cx)
}

/// # Safety
/// `ptr` points at a live `F` placed by `spawn`; the slot is dead after the call.
unsafe fn drop_shim<F>(ptr: *mut u8) {
    // SAFETY: caller contract — `ptr` holds a live `F`; drops it in place.
    unsafe { core::ptr::drop_in_place(ptr.cast::<F>()) }
}

/// Reusable fast-path buffer: kept allocated across `poll_immediate` calls
/// so the Ready-on-first-poll path never calls `malloc`.
struct Scratch {
    ptr: NonNull<u8>,
    layout: Layout,
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if self.layout.size() > 0 {
            // SAFETY: allocated with exactly this layout, owned uniquely.
            unsafe { dealloc(self.ptr.as_ptr(), self.layout) };
        }
    }
}

fn allocate(layout: Layout) -> NonNull<u8> {
    if layout.size() == 0 {
        // Zero-sized futures need no storage; a well-aligned dangling
        // pointer is the canonical stand-in and is never deallocated.
        return NonNull::<u8>::dangling()
            .with_addr(core::num::NonZero::new(layout.align()).expect("layout align is nonzero"));
    }
    // SAFETY: size checked non-zero; layout validity guaranteed by `Layout`.
    let ptr = unsafe { alloc(layout) };
    NonNull::new(ptr).unwrap_or_else(|| handle_alloc_error(layout))
}

/// Scratch sizing: round up so consecutive fast-path commands of slightly
/// different shapes reuse one buffer instead of thrashing the allocator.
fn scratch_layout_for(needed: Layout) -> Layout {
    let size = needed.size().next_power_of_two().max(128);
    let align = needed.align().max(16);
    Layout::from_size_align(size, align).expect("scratch layout")
}

// ---- The executor ----------------------------------------------------------

struct TaskEntry {
    fut: RawFut,
    header: Header,
    generation: u64,
}

/// Single-threaded task executor for one cell. See module docs.
///
/// It stays on the thread that built it, whose mark its headers carry:
///
/// ```compile_fail,E0277
/// fn on_another_thread<T: Send>(_: T) {}
/// on_another_thread(inf_runtime::CellExecutor::new(8));
/// ```
pub struct CellExecutor {
    entries: Vec<Option<TaskEntry>>,
    free: Vec<u32>,
    shared: Rc<ExecShared>,
    /// The remote stack, an allocation of its own: the executor holds one count of it until its
    /// last take, and each closed header one more (ADR-0003 A1.3).
    stack: NonNull<RemoteStack>,
    /// This thread's mark, which every header of this executor carries.
    mark: ThreadMark,
    /// Recycled header for the fast path (no allocation on Ready).
    spare_header: Option<Header>,
    /// Recycled storage for the fast path (no malloc on Ready).
    scratch: Option<Scratch>,
    next_generation: u64,
    live: usize,
    /// `!Send` and `!Sync` by itself: the thread that built the executor is the one that polls
    /// it, so its mark is the owner lane's (ADR-0003 A1.2.3).
    not_send: PhantomData<*const ()>,
}

impl CellExecutor {
    /// `capacity` reserves slab and queue space up front; the slab may still
    /// grow beyond it at M0 (a hard cap with backpressure arrives with the
    /// connection budget work).
    #[allow(clippy::disallowed_types, reason = "container: E")]
    pub fn new(capacity: usize) -> CellExecutor {
        let stack = NonNull::from(Box::leak(Box::new(RemoteStack::new())));
        CellExecutor {
            entries: Vec::with_capacity(capacity),
            free: Vec::new(),
            shared: Rc::new(ExecShared {
                ready: RefCell::new(VecDeque::with_capacity(capacity)),
                held: Cell::new(None),
            }),
            stack,
            mark: ThreadMark::claim(),
            spare_header: None,
            scratch: None,
            next_generation: 0,
            live: 0,
            not_send: PhantomData,
        }
    }

    fn stack_ref(&self) -> &RemoteStack {
        // SAFETY: the executor holds a count of its stack until its drop's last release.
        unsafe { self.stack.as_ref() }
    }

    /// A new header: no waker counted on either lane, no flag, this thread's mark and this
    /// executor's stack.
    fn new_header(&self) -> Header {
        let header = Box::new(WakeHeader {
            bridge: WakeBridge::new(self.stack_ref(), self.mark),
            owner: ManuallyDrop::new(OwnerPart {
                state: Cell::new(TaskState::Dead),
                slot: Cell::new(UNASSIGNED),
                shared: Rc::clone(&self.shared),
                owner_count: Cell::new(0),
                held_prev: Cell::new(None),
                held_next: Cell::new(None),
            }),
        });
        NonNull::from(Box::leak(header))
    }

    /// Fast path (L6): poll `fut` once, in place. Ready ⇒ done — no task
    /// slot, no waker registered, no allocation (the state machine lives in
    /// a reusable scratch buffer). Pending ⇒ the future is promoted to a
    /// task slot and resumes via [`Self::run_ready`] once woken.
    pub fn poll_immediate<F: Future<Output = ()> + 'static>(&mut self, fut: F) -> PollImmediate {
        let header = self.spare_header.take().unwrap_or_else(|| self.new_header());
        // SAFETY: the executor holds the header: open, on this thread.
        let owner = unsafe { owner(header) };
        owner.state.set(TaskState::Running);
        owner.slot.set(UNASSIGNED);

        let needed = Layout::new::<F>();
        let scratch = match self.scratch.take() {
            Some(s) if s.layout.size() >= needed.size() && s.layout.align() >= needed.align() => s,
            stale => {
                drop(stale);
                let layout = scratch_layout_for(needed);
                Scratch { ptr: allocate(layout), layout }
            }
        };

        // Move the future into stable storage BEFORE its first poll — the
        // one ordering Pin demands. From here on it never moves.
        // SAFETY: scratch is sized/aligned for `F` (checked above) and owns
        // uninitialized memory.
        unsafe { core::ptr::write(scratch.ptr.as_ptr().cast::<F>(), fut) };

        let waker = waker_ref(header);
        let mut cx = Context::from_waker(&waker);
        // SAFETY: `scratch.ptr` holds the live `F` just written.
        let poll = unsafe { poll_shim::<F>(scratch.ptr.as_ptr(), &mut cx) };

        match poll {
            Poll::Ready(()) => {
                // SAFETY: future is live and complete; drop it in place and
                // keep the buffer for the next fast-path command. Every waker
                // it owned is counted out here, before the decision.
                unsafe { drop_shim::<F>(scratch.ptr.as_ptr()) };
                self.scratch = Some(scratch);
                owner.state.set(TaskState::Dead);
                self.decide_first_poll(header);
                PollImmediate::Completed
            }
            Poll::Pending => {
                let scratch = ManuallyDrop::new(scratch);
                let raw = RawFut {
                    ptr: scratch.ptr,
                    alloc_layout: scratch.layout,
                    poll_fn: poll_shim::<F>,
                    drop_fn: drop_shim::<F>,
                };
                PollImmediate::Suspended(self.insert(raw, header))
            }
        }
    }

    /// A completed first poll's header, its future dropped: kept spare for the next first poll
    /// when no waker names it and it is not queued; otherwise Held, and the next first poll
    /// takes a new header (a clone escaped a future that completed anyway).
    fn decide_first_poll(&mut self, header: Header) {
        // SAFETY: the executor holds the header: open, on this thread.
        #[cfg(not(inf_canary_ready_ignores_queued))]
        let quiet = unsafe { quiet(header) };
        // Canary: a completing first poll recycles its header with `Q` set.
        // SAFETY: as above.
        #[cfg(inf_canary_ready_ignores_queued)]
        let quiet = unsafe { bridge(header) }
            .unnamed_ignoring_queue(unsafe { owner(header) }.owner_count.get());
        if quiet {
            self.spare_header = Some(header);
        } else {
            // SAFETY: as above; the header is finished and on no list.
            unsafe { link_held(header) };
        }
    }

    /// Spawn a future as a task (the slow path — allocates storage and a
    /// slot). It runs from the next [`Self::run_ready`] slice.
    pub fn spawn_local<F: Future<Output = ()> + 'static>(&mut self, fut: F) -> TaskId {
        let layout = Layout::new::<F>();
        let ptr = allocate(layout);
        // SAFETY: freshly allocated for `F`'s layout.
        unsafe { core::ptr::write(ptr.as_ptr().cast::<F>(), fut) };
        let raw =
            RawFut { ptr, alloc_layout: layout, poll_fn: poll_shim::<F>, drop_fn: drop_shim::<F> };
        let header = self.new_header();
        // `RunningWoken` makes `insert` queue the task for its first poll.
        // SAFETY: a header this executor just made: open, on this thread.
        unsafe { owner(header) }.state.set(TaskState::RunningWoken);
        self.insert(raw, header)
    }

    /// Bind a suspended/new future to a slab slot. Resolves a wake that
    /// raced the first poll (`RunningWoken`) by queueing immediately.
    fn insert(&mut self, fut: RawFut, header: Header) -> TaskId {
        let slot = match self.free.pop() {
            Some(s) => s,
            None => {
                let s = u32::try_from(self.entries.len()).expect("task slab exceeds u32 slots");
                self.entries.push(None);
                s
            }
        };
        // SAFETY: the executor holds the header: open, on this thread.
        let owner = unsafe { owner(header) };
        owner.slot.set(slot);
        match owner.state.get() {
            TaskState::RunningWoken => {
                owner.state.set(TaskState::Queued);
                self.shared.ready.borrow_mut().push_back(slot);
            }
            TaskState::Running => owner.state.set(TaskState::Idle),
            other @ (TaskState::Idle | TaskState::Queued | TaskState::Dead | TaskState::Held) => {
                unreachable!("inserting task in state {other:?}")
            }
        }
        self.next_generation += 1;
        let generation = self.next_generation;
        self.entries[slot as usize] = Some(TaskEntry { fut, header, generation });
        self.live += 1;
        TaskId { slot, generation }
    }

    /// Poll up to `budget` ready tasks (one activation each). Returns how
    /// many were polled — the loop's EXECUTE step charges this against the
    /// foreground deficit. It first delivers the foreign wakes queued on the
    /// remote stack.
    pub fn run_ready(&mut self, budget: usize) -> usize {
        self.drain_remote();
        let mut polled = 0;
        while polled < budget {
            let slot = self.shared.ready.borrow_mut().pop_front();
            let Some(slot) = slot else { break };
            let entry =
                self.entries[slot as usize].as_mut().expect("ready queue references freed slot");
            let header = entry.header;
            // SAFETY: the entry holds its header: open, on this thread.
            let owner = unsafe { owner(header) };
            debug_assert_eq!(owner.state.get(), TaskState::Queued);
            owner.state.set(TaskState::Running);
            let waker = waker_ref(header);
            let mut cx = Context::from_waker(&waker);
            // SAFETY: the entry's future is live and pinned in its buffer.
            let poll = unsafe { entry.fut.poll(&mut cx) };
            polled += 1;
            match poll {
                Poll::Ready(()) => self.release(slot),
                Poll::Pending => match owner.state.get() {
                    TaskState::RunningWoken => {
                        owner.state.set(TaskState::Queued);
                        self.shared.ready.borrow_mut().push_back(slot);
                    }
                    TaskState::Running => owner.state.set(TaskState::Idle),
                    other @ (TaskState::Idle
                    | TaskState::Queued
                    | TaskState::Dead
                    | TaskState::Held) => {
                        unreachable!("post-poll task state {other:?}")
                    }
                },
            }
        }
        polled
    }

    /// The drain (ADR-0003 A1.2.7): when the remote stack is not empty, takes all of it; each
    /// bridge's link is read before its `Q` and `W` are cleared. A cleared `W` is the owner-lane
    /// wake of that header's task; then a Held header no waker names is unlinked and freed.
    fn drain_remote(&self) {
        let stack = self.stack_ref();
        if !stack.pending() {
            return;
        }
        let mut drained = stack.take();
        // SAFETY: a queued bridge is allocated while its `Q` is set: the push's node, the
        // header's own address.
        while let Some((at, delivery)) = drained.deliver_next(|at| unsafe { &*at.as_ptr() }) {
            // SAFETY: the node is the header's address (`repr(C)`, bridge first), with its
            // provenance; the executor lives, so the header is open, and this is its thread.
            unsafe { settle_drained(at.cast::<WakeHeader>(), delivery) };
        }
    }

    fn release(&mut self, slot: u32) {
        let entry = self.entries[slot as usize].take().expect("releasing freed slot");
        // SAFETY: the entry held its header: open, on this thread.
        let owner = unsafe { owner(entry.header) };
        owner.state.set(TaskState::Dead);
        owner.slot.set(UNASSIGNED);
        self.free.push(slot);
        self.live -= 1;
        // The future drops in place here (drop_in_place + dealloc): every waker it owned is
        // counted out before the decision.
        drop(entry.fut);
        // SAFETY: as above; its future is dropped, so the header is decided once, here.
        unsafe { decide_released(entry.header) };
    }

    /// Live (suspended or queued) tasks — slab occupancy. The leak assert:
    /// after a quiesced workload this must be 0.
    pub fn live_tasks(&self) -> usize {
        self.live
    }

    /// Finished tasks' headers a waker still names or a foreign wake still holds: the held list,
    /// walked once (O(held), a diagnostic's cost; the list itself links and unlinks in O(1)).
    pub fn held_headers(&self) -> usize {
        let mut count = 0;
        let mut at = self.shared.held.get();
        while let Some(header) = at {
            count += 1;
            // SAFETY: the held list links open headers, on this thread.
            at = unsafe { owner(header) }.held_next.get();
        }
        count
    }

    /// Foreign-lane operations on this cell's headers (`waker_foreign_ops`, cell scope): 0
    /// while no product path moves a waker off its cell (ADR-0149 D1). A statistic: it wraps.
    pub fn waker_foreign_ops(&self) -> u64 {
        self.stack_ref().foreign_ops()
    }

    /// Whether a task is queued to run — work the loop's park decision must see: a task woken
    /// after `run_ready`, or a foreign wake on the remote stack, which the next `run_ready`
    /// delivers.
    pub fn has_ready(&self) -> bool {
        !self.shared.ready.borrow().is_empty() || self.stack_ref().pending()
    }

    /// Whether `id` still names a live task (slots are recycled; stale ids
    /// compare against the generation).
    pub fn is_live(&self, id: TaskId) -> bool {
        self.entries
            .get(id.slot as usize)
            .and_then(Option::as_ref)
            .is_some_and(|e| e.generation == id.generation)
    }

    /// The drop's step 3: closes `header` (ADR-0003 A1.3): counted on the stack, its `o` read,
    /// its owner part dropped in place, then `MARK_CLOSED` and the fold of `o` into `c` with `X`.
    /// Freed now if no waker names it and no push holds it; otherwise its last waker's drop, on
    /// any thread, frees it.
    fn close(&self, header: Header) {
        self.stack_ref().retain();
        // SAFETY: the executor holds the header: open, on this thread, unlinked by the caller.
        let owner_count = unsafe { owner(header) }.owner_count.get();
        if !PLANT_CLOSE_KEEPS_OWNER_PART {
            // SAFETY: as above; the last owner-part access, which releases its `Rc`.
            unsafe { ManuallyDrop::drop(&mut (*header.as_ptr()).owner) };
        }
        // SAFETY: the bridge outlives the owner part; atomics only.
        match unsafe { bridge(header) }.close(owner_count) {
            // SAFETY: `close` left it at `c = 0` with `Q` clear: this thread frees it.
            Closing::Free => unsafe { free_closed(header) },
            Closing::Retain => {}
        }
    }
}

/// One drained bridge of a live executor: a pending wake delivered on the owner lane, then a Held
/// header no waker names freed.
///
/// # Safety
/// `header` is open, its `Q` just cleared by this drain, and the caller is the owner thread.
unsafe fn settle_drained(header: Header, delivery: Delivery) {
    // SAFETY: per this function's contract.
    let owner = unsafe { owner(header) };
    match delivery {
        Delivery::Wake => wake_task(owner),
        Delivery::NoWake => {}
    }
    let held = owner.state.get() == TaskState::Held;
    // SAFETY: as above; `owner` is not used after the free.
    if held && unsafe { bridge(header) }.quiet(owner.owner_count.get()) {
        // SAFETY: Held, unnamed and unqueued: the list's to unlink, then free.
        unsafe {
            unlink_held(header);
            free_open(header);
        }
    }
}

/// A released task's header, its future dropped: freed when no waker names it and it is not
/// queued; otherwise Held.
///
/// # Safety
/// `header` is open, finished (Dead) and on no list; the caller is the owner thread.
unsafe fn decide_released(header: Header) {
    // SAFETY: per this function's contract.
    if unsafe { quiet(header) } {
        // SAFETY: unnamed and unqueued.
        unsafe { free_open(header) };
    } else {
        // SAFETY: per this function's contract.
        unsafe { link_held(header) };
    }
}

impl Drop for CellExecutor {
    /// The executor's drop (ADR-0003 A1.3): it drains; marks every task's header Dead and drops
    /// the futures, deciding each header after its future as `release` does; closes every header
    /// still held, the held list's and the spare; takes the stack a last time, leaving its head
    /// closed; then releases its own count of the stack and, the last, its `Rc` of the shared
    /// state. Nothing is kept for the process: a closed header lives until its last waker's drop.
    fn drop(&mut self) {
        self.drain_remote();
        for entry in self.entries.iter().flatten() {
            // SAFETY: the entry holds its header: open, on this thread.
            unsafe { owner(entry.header) }.state.set(TaskState::Dead);
        }
        for slot in 0..self.entries.len() {
            if let Some(entry) = self.entries[slot].take() {
                drop(entry.fut);
                // SAFETY: its future is dropped: the header is decided once, here.
                unsafe { decide_released(entry.header) };
            }
        }
        while let Some(header) = self.shared.held.get() {
            // SAFETY: the held list links open headers, on this thread.
            unsafe { unlink_held(header) };
            self.close(header);
        }
        if let Some(spare) = self.spare_header.take() {
            self.close(spare);
        }
        let mut last = self.stack_ref().take_last();
        // SAFETY: a queued bridge is allocated while its `Q` is set.
        while let Some((at, settled)) = last.settle_next(|at| unsafe { &*at.as_ptr() }) {
            match settled {
                // SAFETY: the last take's clear left it at `c = 0` with `Q` clear.
                Settled::Last => unsafe { free_closed(at.cast::<WakeHeader>()) },
                Settled::Kept => {}
            }
        }
        // SAFETY: the executor's own count, held until here.
        unsafe { foreign::release_stack(self.stack) };
        // The fields drop after this: the shared state's last `Rc`, the scratch block and the
        // slab's storage.
    }
}

impl core::fmt::Debug for CellExecutor {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "CellExecutor {{ live: {}, ready: {}, slots: {}, held: {} }}",
            self.live,
            self.shared.ready.borrow().len(),
            self.entries.len(),
            self.held_headers()
        )
    }
}

#[cfg(test)]
mod tests {
    //! The owner lane's bound, which no public path reaches before 2⁶⁰ clones.

    use super::*;

    /// An owner-lane clone at `LANE_COUNT_MAX` aborts the process before the count is written;
    /// one below the bound reaches it. A test-only start count stands in for 2⁶⁰ clones: the test
    /// runs its own binary on itself, the child cloning at the bound, and requires the child to
    /// die by `SIGABRT`.
    #[test]
    #[cfg(unix)]
    #[cfg_attr(miri, ignore = "the test spawns a process, which Miri does not")]
    fn an_owner_lane_clone_at_the_bound_aborts() {
        use std::os::unix::process::ExitStatusExt;
        const CHILD: &str = "INF_OWNER_BOUND_CHILD";
        const NAME: &str = "executor::tests::an_owner_lane_clone_at_the_bound_aborts";
        /// `SIGABRT`'s number on every supported host.
        const SIGABRT: i32 = 6;
        let start = if std::env::var(CHILD).is_ok() { LANE_COUNT_MAX } else { LANE_COUNT_MAX - 1 };
        let mut executor = CellExecutor::new(1);
        let header = executor.new_header();
        // SAFETY: a header this executor just made: open, on this thread.
        unsafe { owner(header) }.owner_count.set(start);
        executor.spare_header = Some(header);
        let reached = Rc::new(Cell::new(0));
        let seen = Rc::clone(&reached);
        let outcome = executor.poll_immediate(core::future::poll_fn(move |cx| {
            let clone = cx.waker().clone();
            // SAFETY: the polled task's header, on this thread.
            seen.set(unsafe { owner(header) }.owner_count.get());
            drop(clone);
            Poll::Ready(())
        }));
        if std::env::var(CHILD).is_ok() {
            return;
        }
        assert_eq!(outcome, PollImmediate::Completed);
        assert_eq!(reached.get(), LANE_COUNT_MAX, "one below the bound, a clone reaches it");
        let exe = std::env::current_exe().expect("the test binary's path");
        let output = std::process::Command::new(exe)
            .args(["--exact", NAME, "--nocapture", "--test-threads=1"])
            .env(CHILD, "clone")
            .output()
            .expect("the child test runs");
        assert_eq!(
            output.status.signal(),
            Some(SIGABRT),
            "the owner lane's child exited {:?}, not by SIGABRT",
            output.status
        );
    }
}
