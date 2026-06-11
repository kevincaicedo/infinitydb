//! The cell executor (ADR-0003): single-threaded, `!Send` futures, Rc-based
//! wakers with **no atomic instructions anywhere in the waker path**, and a
//! fast path that costs (nearly) nothing — a future completing on its first
//! poll never allocates a task slot and never touches `malloc` (its state
//! machine is placed into a reusable scratch buffer).
//!
//! Suspension is the slow path by design: fabric hops (M0), blocking ops
//! (M1), fsync gating (M2) and cold-tier reads (M7) all promote the future
//! into a slab slot and park it on a typed gate (see [`crate::gate`]).
//!
//! # Thread-locality contract (the one big invariant)
//!
//! Wakers built here use non-atomic `Rc` reference counts and `Cell` state.
//! [`std::task::RawWakerVTable`]'s documented contract requires wakers to be
//! thread-safe; these are **deliberately not** (L1: one thread owns a cell,
//! atomics would be pure overhead). Sending a waker clone to another thread
//! is undefined behavior. This is enforced by discipline, not the type
//! system: cell code cannot name `std::sync`/`tokio` (deny-list, M0-S06),
//! futures themselves are `!Send` by construction, and DST (M0-S20)
//! exercises the interleavings. See `SAFETY.md`.

use core::alloc::Layout;
use core::cell::{Cell, RefCell};
use core::future::Future;
use core::mem::ManuallyDrop;
use core::pin::Pin;
use core::ptr::NonNull;
use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
use std::alloc::{alloc, dealloc, handle_alloc_error};
use std::collections::VecDeque;
use std::rc::Rc;

/// Slot value while a task header is not (yet) bound to a slab slot.
const UNASSIGNED: u32 = u32::MAX;

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
    /// Completed or recycled; wakes are no-ops.
    Dead,
}

/// Ready queue shared between the executor and every waker. `Rc<RefCell<…>>`
/// — single-threaded interior mutability, no atomics (L1).
type ReadyQueue = Rc<RefCell<VecDeque<u32>>>;

/// Per-task shared state. Wakers are `Rc<TaskHeader>` behind a raw vtable.
struct TaskHeader {
    state: Cell<TaskState>,
    slot: Cell<u32>,
    ready: ReadyQueue,
}

/// The wake transition. Runs inside `Waker::wake`, so it must not touch the
/// executor itself — only the header and the shared ready queue.
fn wake_header(header: &TaskHeader) {
    match header.state.get() {
        TaskState::Idle => {
            let slot = header.slot.get();
            debug_assert_ne!(slot, UNASSIGNED, "idle task without a slot");
            header.state.set(TaskState::Queued);
            header.ready.borrow_mut().push_back(slot);
        }
        // Woken during its own poll (wake-before-suspend): remember it so
        // the executor requeues on `Pending` — the classic lost-wakeup hole.
        TaskState::Running => header.state.set(TaskState::RunningWoken),
        // Already queued / already flagged / completed: wakes coalesce.
        TaskState::Queued | TaskState::RunningWoken | TaskState::Dead => {}
    }
}

// ---- Rc waker vtable (no atomics; see module docs + SAFETY.md) ------------

unsafe fn waker_clone(data: *const ()) -> RawWaker {
    // SAFETY: `data` originates from `Rc::as_ptr` on a live `Rc<TaskHeader>`
    // (waker_ref) or from a previous clone; incrementing the non-atomic
    // strong count keeps the header alive for the new waker. Same thread by
    // the thread-locality contract.
    unsafe { Rc::increment_strong_count(data.cast::<TaskHeader>()) };
    RawWaker::new(data, &WAKER_VTABLE)
}

unsafe fn waker_wake(data: *const ()) {
    // SAFETY: `data` is a live `Rc<TaskHeader>` raw pointer owned by this
    // waker; we consume the waker, so we also drop its strong count.
    unsafe {
        wake_header(&*data.cast::<TaskHeader>());
        Rc::decrement_strong_count(data.cast::<TaskHeader>());
    }
}

unsafe fn waker_wake_by_ref(data: *const ()) {
    // SAFETY: as in `waker_wake`, minus consuming the reference.
    unsafe { wake_header(&*data.cast::<TaskHeader>()) };
}

unsafe fn waker_drop(data: *const ()) {
    // SAFETY: drops the strong count this waker owned.
    unsafe { Rc::decrement_strong_count(data.cast::<TaskHeader>()) };
}

static WAKER_VTABLE: RawWakerVTable =
    RawWakerVTable::new(waker_clone, waker_wake, waker_wake_by_ref, waker_drop);

/// A `Waker` view over a borrowed `Rc<TaskHeader>` without refcount churn:
/// the executor polls with this; the future only pays an `Rc` increment if
/// it actually clones the waker (i.e. it is really suspending).
fn waker_ref(header: &Rc<TaskHeader>) -> ManuallyDrop<Waker> {
    let raw = RawWaker::new(Rc::as_ptr(header).cast::<()>(), &WAKER_VTABLE);
    // SAFETY: the vtable above pairs every count increment/decrement;
    // `ManuallyDrop` keeps this borrowed view from decrementing a count it
    // never incremented. Thread-locality per the module contract.
    ManuallyDrop::new(unsafe { Waker::from_raw(raw) })
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

unsafe fn poll_shim<F: Future<Output = ()>>(ptr: *mut u8, cx: &mut Context<'_>) -> Poll<()> {
    // SAFETY: caller contract — `ptr` holds a live `F`, pinned in place
    // since before its first poll.
    let fut = unsafe { &mut *ptr.cast::<F>() };
    // SAFETY: the storage never moves (RawFut owns it; slab stores RawFut by
    // value but the buffer it points to is heap-stable).
    unsafe { Pin::new_unchecked(fut) }.poll(cx)
}

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
    header: Rc<TaskHeader>,
    generation: u64,
}

/// Single-threaded task executor for one shard cell. See module docs.
pub struct CellExecutor {
    entries: Vec<Option<TaskEntry>>,
    free: Vec<u32>,
    ready: ReadyQueue,
    /// Recycled header for the fast path (no allocation on Ready).
    spare_header: Option<Rc<TaskHeader>>,
    /// Recycled storage for the fast path (no malloc on Ready).
    scratch: Option<Scratch>,
    next_generation: u64,
    live: usize,
}

impl CellExecutor {
    /// `capacity` reserves slab and queue space up front; the slab may still
    /// grow beyond it at M0 (a hard cap with backpressure arrives with the
    /// connection budget work).
    pub fn new(capacity: usize) -> CellExecutor {
        CellExecutor {
            entries: Vec::with_capacity(capacity),
            free: Vec::new(),
            ready: Rc::new(RefCell::new(VecDeque::with_capacity(capacity))),
            spare_header: None,
            scratch: None,
            next_generation: 0,
            live: 0,
        }
    }

    fn new_header(&self) -> Rc<TaskHeader> {
        Rc::new(TaskHeader {
            state: Cell::new(TaskState::Dead),
            slot: Cell::new(UNASSIGNED),
            ready: Rc::clone(&self.ready),
        })
    }

    /// Fast path (L6): poll `fut` once, in place. Ready ⇒ done — no task
    /// slot, no waker registered, no allocation (the state machine lives in
    /// a reusable scratch buffer). Pending ⇒ the future is promoted to a
    /// task slot and resumes via [`Self::run_ready`] once woken.
    pub fn poll_immediate<F: Future<Output = ()> + 'static>(&mut self, fut: F) -> PollImmediate {
        let header = self.spare_header.take().unwrap_or_else(|| self.new_header());
        header.state.set(TaskState::Running);
        header.slot.set(UNASSIGNED);

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

        let waker = waker_ref(&header);
        let mut cx = Context::from_waker(&waker);
        // SAFETY: `scratch.ptr` holds the live `F` just written.
        let poll = unsafe { poll_shim::<F>(scratch.ptr.as_ptr(), &mut cx) };

        match poll {
            Poll::Ready(()) => {
                // SAFETY: future is live and complete; drop it in place and
                // keep the buffer for the next fast-path command.
                unsafe { drop_shim::<F>(scratch.ptr.as_ptr()) };
                self.scratch = Some(scratch);
                if Rc::strong_count(&header) == 1 {
                    header.state.set(TaskState::Dead);
                    self.spare_header = Some(header);
                } else {
                    // A waker clone escaped from a future that then
                    // completed anyway; keep the header alive (clones make
                    // dead wakes no-ops) and mint a fresh one next time.
                    header.state.set(TaskState::Dead);
                }
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
        header.state.set(TaskState::RunningWoken);
        self.insert(raw, header)
    }

    /// Bind a suspended/new future to a slab slot. Resolves a wake that
    /// raced the first poll (`RunningWoken`) by queueing immediately.
    fn insert(&mut self, fut: RawFut, header: Rc<TaskHeader>) -> TaskId {
        let slot = match self.free.pop() {
            Some(s) => s,
            None => {
                let s = u32::try_from(self.entries.len()).expect("task slab exceeds u32 slots");
                self.entries.push(None);
                s
            }
        };
        header.slot.set(slot);
        match header.state.get() {
            TaskState::RunningWoken => {
                header.state.set(TaskState::Queued);
                self.ready.borrow_mut().push_back(slot);
            }
            TaskState::Running => header.state.set(TaskState::Idle),
            other => unreachable!("inserting task in state {other:?}"),
        }
        self.next_generation += 1;
        let generation = self.next_generation;
        self.entries[slot as usize] = Some(TaskEntry { fut, header, generation });
        self.live += 1;
        TaskId { slot, generation }
    }

    /// Poll up to `budget` ready tasks (one activation each). Returns how
    /// many were polled — the loop's EXECUTE step charges this against the
    /// foreground deficit.
    pub fn run_ready(&mut self, budget: usize) -> usize {
        let mut polled = 0;
        while polled < budget {
            let slot = self.ready.borrow_mut().pop_front();
            let Some(slot) = slot else { break };
            let entry =
                self.entries[slot as usize].as_mut().expect("ready queue references freed slot");
            debug_assert_eq!(entry.header.state.get(), TaskState::Queued);
            entry.header.state.set(TaskState::Running);
            let header = Rc::clone(&entry.header);
            let waker = waker_ref(&header);
            let mut cx = Context::from_waker(&waker);
            // SAFETY: the entry's future is live and pinned in its buffer.
            let poll = unsafe { entry.fut.poll(&mut cx) };
            polled += 1;
            match poll {
                Poll::Ready(()) => self.release(slot),
                Poll::Pending => match header.state.get() {
                    TaskState::RunningWoken => {
                        header.state.set(TaskState::Queued);
                        self.ready.borrow_mut().push_back(slot);
                    }
                    TaskState::Running => header.state.set(TaskState::Idle),
                    other => unreachable!("post-poll task state {other:?}"),
                },
            }
        }
        polled
    }

    fn release(&mut self, slot: u32) {
        let entry = self.entries[slot as usize].take().expect("releasing freed slot");
        entry.header.state.set(TaskState::Dead);
        entry.header.slot.set(UNASSIGNED);
        self.free.push(slot);
        self.live -= 1;
        // `entry.fut` drops here: drop_in_place + dealloc.
    }

    /// Live (suspended or queued) tasks — slab occupancy. The leak assert:
    /// after a quiesced workload this must be 0.
    pub fn live_tasks(&self) -> usize {
        self.live
    }

    /// Whether `id` still names a live task (slots are recycled; stale ids
    /// compare against the generation).
    pub fn is_live(&self, id: TaskId) -> bool {
        self.entries
            .get(id.slot as usize)
            .and_then(Option::as_ref)
            .is_some_and(|e| e.generation == id.generation)
    }
}

impl core::fmt::Debug for CellExecutor {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "CellExecutor {{ live: {}, ready: {}, slots: {} }}",
            self.live,
            self.ready.borrow().len(),
            self.entries.len()
        )
    }
}
