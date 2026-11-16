//! The cell executor (ADR-0003, ADR-0177 D2): single-threaded and `!Send`. Every task belongs to
//! a class, a fixed slab of the class's one future type, built before its cell serves: the
//! future is placed in its slot before its first poll, never moves, and is dropped in place. A
//! first poll runs in its class's **spare**, one live slot holding a husk that no task holds and
//! no id names, reached through the address the slab last lent while nothing else has borrowed
//! the slab ([`class`]). A future Ready on its first poll leaves that slot the spare (the fast
//! path: no slab call, no allocation); one that suspends takes it as its own and resumes there
//! when woken, by a waker (typed gates, [`crate::gate`]) or by its owner through
//! [`CellExecutor::wake`].
//!
//! # The header array
//!
//! One allocation per executor holds the remote stack, the ready list's two ends and one wake
//! header per class slot, parallel to the class slabs: class `j`'s slot `i` is header
//! `base_j + i`. The ready list is intrusive, a FIFO linked through the headers by index, so a
//! wake allocates nothing and a header is on it at most once. The array is freed whole, with the
//! stack, by the release of its last count: the executor's own after its drop, or a closed
//! header's at its last waker's drop, on whichever thread drops it.
//!
//! # A standard `Waker` at a wake bridge (ADR-0003 A1)
//!
//! A `Waker` is `Send + Sync`, so its four operations must be sound on any thread, whatever the
//! future that cloned it. A waker's data is its task's **wake header**: first the
//! [`WakeBridge`], atomics only, which any thread may touch; then the header's place in its
//! array, written once at the array's build; then the owner part (the task's state and slot, the
//! owner lane's count `o` and the ready list's link), which only the thread that built the
//! executor reaches. Each operation compares the bridge's mark with the calling thread's
//! ([`owner_part`]):
//!
//! - **the owner lane** (the marks match): the single-threaded transition, on `Cell`s, with no
//!   atomic read-modify-write, fence or ordered access (ADR-0003 A1.5); `o` is compared with
//!   `LANE_COUNT_MAX` before it grows;
//! - **the foreign lane** (any other thread, and every thread once the executor is gone):
//!   [`foreign`], five `#[inline(never)]` functions over `inf_foundation::wake`. A foreign wake
//!   is queued on the executor's remote stack and delivered by the next [`CellExecutor::run_ready`]
//!   (its drain); [`CellExecutor::has_ready`] sees it first.
//!
//! A finished task's header is decided only after its future's drop: its slot frees at once, or
//! it is **Retired** — the slot stays held — while a waker names the header or a foreign wake for
//! it is queued. The last such waker's drop links a reclaim entry on the ready list, and the next
//! `run_ready` frees the slot; a slot is reused only with no waker of either lane naming its
//! header. The executor's drop closes every header a waker still names. No product path moves a
//! waker off its cell (ADR-0149 D1); the foreign lane makes a violation a counted, sound wake
//! (`waker_foreign_ops`). `SAFETY.md` holds the audit of this module's `unsafe`.

use core::alloc::Layout;
use core::any::TypeId;
use core::cell::{Cell, UnsafeCell};
use core::future::Future;
use core::marker::PhantomData;
use core::mem::ManuallyDrop;
use core::pin::Pin;
use core::ptr::NonNull;
use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
use std::alloc::{alloc, dealloc};
use std::rc::Rc;

use inf_foundation::bounded::{
    Built, Cap, CapCensus, CapError, CappedSlab, Lookup, LookupMut, Released, Retire, Retiring,
    SlabBuilder, SlabInstance, SlotId,
};
use inf_foundation::limits::{LANE_COUNT_MAX, chunk_slots_for};
use inf_foundation::wake::{
    Closing, Delivery, Freed, RemoteStack, Settled, ThreadMark, WakeBridge,
};

use crate::limits::TASK_CLASSES_MAX;

/// A ready-list link that names no header.
const NO_HEADER: u32 = u32::MAX;

// ---- Planted violations (canaries; each a `cfg`, never a feature) -------------------------

/// The lane compare answers the owner part on every thread.
const PLANT_OWNER_UNCHECKED: bool = cfg!(inf_canary_waker_owner_unchecked);
/// The lane compare answers no owner part on any thread: every operation is foreign.
const PLANT_FOREIGN_LANE_ALWAYS: bool = cfg!(inf_canary_waker_foreign_lane_always);
/// A finished header's decision reads `o` and not the lane word.
const PLANT_REUSE_OWNER_COUNT_ONLY: bool = cfg!(inf_canary_wake_reuse_owner_count_only);
/// The swap that leaves a closed header at `c = 0` with `Q` clear releases nothing.
const PLANT_CLOSED_HEADER_KEPT: bool = cfg!(inf_canary_closed_header_kept);
/// The owner lane compares no count with `LANE_COUNT_MAX` (the foreign lane's half is
/// `inf_foundation::wake`'s).
const PLANT_COUNT_UNBOUNDED: bool = cfg!(inf_canary_wake_count_unbounded);
/// An owner-lane drop settles a Finished header too, not only a Retired one.
const PLANT_SETTLE_ON_DEAD: bool = cfg!(inf_canary_settle_on_dead);
/// An owner-lane drop of a Retired header's last waker does not settle it.
const PLANT_SETTLE_HELD_SKIPPED: bool = cfg!(inf_canary_settle_held_skipped);
/// A finished task's slot frees with a waker still naming its header.
const PLANT_REUSED_WITH_WAKERS: bool = cfg!(inf_canary_header_reused_with_wakers);
/// `reserve` skips the class slab's `SlabInstance` compare.
const PLANT_INSTANCE_BLIND: bool = cfg!(inf_canary_task_class_instance_blind);
/// A Pending first poll leaves its header Idle and unlinked, even when the poll woke it.
const PLANT_FIRST_POLL_WAKE_DROPPED: bool = cfg!(inf_canary_first_poll_wake_dropped);
/// A Pending first poll takes the spare's handle for its id but leaves the spare set.
const PLANT_SPARE_KEPT_PAST_SUSPEND: bool = cfg!(inf_canary_spare_kept_past_suspend);

/// Identifies a task while it is live: its class and its slot's handle, whose generation makes a
/// stale id detectable (`CellExecutor::is_live`) and whose instance names the slab that minted it.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct TaskId {
    class: u32,
    slot: SlotId<u64>,
}

impl TaskId {
    /// The generation of the slot the id names: each reuse of a slot advances it once
    /// (ADR-0177 D7), so one slot's successive occupants' ids differ by one each.
    #[must_use]
    pub fn generation(self) -> u64 {
        self.slot.generation()
    }
}

/// Outcome of [`TaskPermit::poll_immediate`].
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum PollImmediate {
    /// The future finished on its first poll — the fast path: its slot stays its class's spare,
    /// or, while a waker the future left names its header, is held Retired.
    Completed,
    /// The future suspended in its slot; it resumes via its waker or its owner's
    /// [`CellExecutor::wake`] and [`CellExecutor::run_ready`].
    Suspended(TaskId),
}

// ---- Task classes (ADR-0177 D2) ----------------------------------------------------------------

/// What a class's factory makes, read from the factory's type alone: a function or closure of one
/// to three arguments whose call returns the class's future. `ExecutorLayout::class` reads the
/// future's type through it and never calls the factory.
pub trait TaskFactory<Args>: factory::Sealed<Args> {
    /// The future one call of the factory returns: the class's one future type.
    type Future: Future<Output = ()> + 'static;
}

mod factory {
    /// Sealed: only the arities below are factories.
    pub trait Sealed<Args> {}
}

macro_rules! task_factory {
    ($($arg:ident),+) => {
        impl<Fun, Fut, $($arg),+> factory::Sealed<($($arg,)+)> for Fun
        where
            Fun: Fn($($arg),+) -> Fut,
        {
        }
        impl<Fun, Fut, $($arg),+> TaskFactory<($($arg,)+)> for Fun
        where
            Fun: Fn($($arg),+) -> Fut,
            Fut: Future<Output = ()> + 'static,
        {
            type Future = Fut;
        }
    };
}
task_factory!(A);
task_factory!(A, B);
task_factory!(A, B, C);

/// A task class's handle (ADR-0177 D2): `Copy` and erased, so a host that cannot name its
/// future's type keeps it. Only [`ExecutorLayout::class`] makes one; it carries its class slab's
/// `SlabInstance`, so [`CellExecutor::reserve`] answers `Foreign` for a class of another
/// executor, two built alike included (ADR-0171 A2.1), and for another future type by the
/// `TypeId` the class's entry stores.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct TaskClass {
    index: u32,
    instance: SlabInstance,
}

/// The answer to [`CellExecutor::reserve`]: a permit for one task of the class, or why none.
#[must_use = "a permit makes a task or is dropped; a refusal is the caller's crossing"]
pub enum TaskAdmission<'a, F> {
    /// The class's spare is the permit's until it makes the task or drops.
    Granted(TaskPermit<'a, F>),
    /// Every slot of the class that can serve holds a task and none is its spare; nothing was
    /// taken, and the class's census row counts the crossing.
    Full,
    /// Every slot of the class has retired: the cell's fail-stop (ADR-0177 D7).
    Exhausted,
    /// The handle names another executor's class or another future type: an assembly defect,
    /// never load; nothing was taken.
    Foreign,
}

/// An exclusive permit for one task of a class: its class's spare, lent. It borrows the
/// executor, so it is used in the step that reserved it and cannot cross an `await` (ADR-0144
/// A2's list names it). Dropped, its slot stays the class's spare and no future was built. Five
/// words, kept in registers where `reserve` and its use inline.
#[must_use = "a permit makes a task or is dropped; it carries none"]
pub struct TaskPermit<'a, F> {
    /// The spare's occupant, the husk, where it lies.
    cell: &'a mut TaskCell<F>,
    /// The spare's handle: a task takes it with `take()`, so no id carries it before then.
    spare: &'a mut Option<SlotId<u64>>,
    /// The class's Retired count.
    retired: &'a mut u32,
    /// The spare's header: Free and on no list.
    header: Header,
    class: u32,
}

impl<F: Future<Output = ()> + 'static> TaskPermit<'_, F> {
    /// Makes the task now: `make` runs, its future is placed in the spare's slot before any poll,
    /// the slot becomes the task's, and the task is polled by the next
    /// [`CellExecutor::run_ready`].
    #[inline(always)]
    pub fn spawn_local(self, make: impl FnOnce() -> F) -> TaskId {
        let TaskPermit { cell, spare, header, class, .. } = self;
        // The husk has no drop glue: the write drops nothing.
        *cell = TaskCell::Live(UnsafeCell::new(ManuallyDrop::new(make())));
        let Some(slot) = spare.take() else { unreachable!("a permit holds its class's spare") };
        // SAFETY: the spare's header: open, on this thread, and on no list.
        let owner = unsafe { owner(header) };
        owner.slot.set(Some(slot));
        owner.state.set(TaskState::Queued);
        // SAFETY: as above.
        unsafe { link_ready(header) };
        TaskId { class, slot }
    }

    /// Makes the task and polls it once in place, in the spare's slot (the fast path, L6):
    /// Ready, its future is dropped in place and the slot stays the spare, or is Retired while a
    /// waker the future left names its header; Pending, the slot is the task's, which resumes
    /// there when woken.
    #[inline(always)]
    pub fn poll_immediate(self, make: impl FnOnce() -> F) -> PollImmediate {
        let TaskPermit { cell, spare, retired, header, class } = self;
        // The husk has no drop glue: the write drops nothing.
        *cell = TaskCell::Live(UnsafeCell::new(ManuallyDrop::new(make())));
        // SAFETY: the spare's header: open, on this thread, and on no list.
        unsafe { owner(header) }.state.set(TaskState::Running);
        let waker = waker_ref(header);
        let mut cx = Context::from_waker(&waker);
        // SAFETY: the slot's own header, Running; its future was just placed in the slot.
        match unsafe { poll_in_place(cell, header, &mut cx) } {
            // SAFETY: as above; the future was dropped in place, so its wakers are counted out.
            Poll::Ready(()) if PLANT_REUSED_WITH_WAKERS || unsafe { quiet(header) } => {
                // No id names the slot's generation: it stays the spare, unadvanced.
                // SAFETY: as above.
                unsafe { owner(header) }.state.set(TaskState::Free);
                PollImmediate::Completed
            }
            // SAFETY: as above.
            Poll::Ready(()) => unsafe { first_retired(header, spare, retired) },
            // SAFETY: as above; the header is on no list.
            Poll::Pending => unsafe { first_pending(header, spare, class) },
        }
    }
}

/// A first poll Ready while a waker the future left names its header (E's Retired row, off the
/// fast path): the spare's handle leaves it for the Retired header, which holds the slot until
/// that waker's drop links its reclaim.
///
/// # Safety
/// `header` is the spare's, open, on the owner thread, its future just dropped in place.
#[cold]
#[inline(never)]
unsafe fn first_retired(
    header: Header,
    spare: &mut Option<SlotId<u64>>,
    retired: &mut u32,
) -> PollImmediate {
    // SAFETY: per the contract.
    let owner = unsafe { owner(header) };
    owner.slot.set(spare.take());
    owner.state.set(TaskState::Retired);
    *retired += 1;
    PollImmediate::Completed
}

/// A first poll Pending (off the fast path): the spare's handle leaves it as the task's id and
/// its header's slot, and the header is Idle, or Queued when the poll woke it. One canary leaves
/// the spare set, so the next first poll writes over the suspended task.
///
/// # Safety
/// `header` is the spare's, open, on the owner thread, and on no list.
#[cold]
#[inline(never)]
unsafe fn first_pending(
    header: Header,
    spare: &mut Option<SlotId<u64>>,
    class: u32,
) -> PollImmediate {
    let held = if PLANT_SPARE_KEPT_PAST_SUSPEND { *spare } else { spare.take() };
    let Some(slot) = held else { unreachable!("a permit holds its class's spare") };
    // SAFETY: per the contract.
    let owner = unsafe { owner(header) };
    owner.slot.set(Some(slot));
    if PLANT_FIRST_POLL_WAKE_DROPPED {
        owner.state.set(TaskState::Idle);
    } else {
        // SAFETY: per the contract.
        unsafe { suspended(header) };
    }
    PollImmediate::Suspended(TaskId { class, slot })
}

/// The answer to [`CellExecutor::wake`].
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Woken {
    /// The task was Idle and is now at the ready list's tail.
    Queued,
    /// The task was queued or running already: the wakes coalesce.
    AlreadyRunnable,
    /// The id names no live task of this executor: nothing changed.
    Stale,
}

/// A leaf future that suspends once and stores no waker (ADR-0177 D2): its task is woken by its
/// owner's [`CellExecutor::wake`], never by a waker.
#[must_use = "a park suspends only when awaited"]
#[derive(Debug, Default)]
pub struct Park {
    parked: bool,
}

/// Suspends the calling task once, until its owner wakes it by id.
pub fn park() -> Park {
    Park { parked: false }
}

impl Future for Park {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<()> {
        if self.parked {
            Poll::Ready(())
        } else {
            self.parked = true;
            Poll::Pending
        }
    }
}

/// A class slot's value. `Live` holds the future, placed before its first poll, polled where it
/// lies and dropped there; `Done` is the husk after that drop, which the slot keeps as its
/// class's spare or until the task's decision releases it, dropped where it lies (ADR-0194 D1's
/// second note). Neither variant has drop glue: the slab never drops a future.
///
/// The future is in an `UnsafeCell`: a suspended future may hold `&mut` into itself across an
/// `await`, and a slab lookup reborrows a whole chunk shared (a slice's length, a shared walk),
/// which, over bytes outside an `UnsafeCell`, is a read that the aliasing model lets invalidate
/// that `&mut` in every slot of the chunk. Inside the cell a shared reborrow freezes nothing, so
/// a lookup of one slot leaves every future in its chunk as it was (`SAFETY.md` §3).
enum TaskCell<F> {
    Live(UnsafeCell<ManuallyDrop<F>>),
    Done,
}

/// A class slab: one slot store of the class's one future type, at the chunk its slot's bytes
/// take (ADR-0194 A1), its generation `u64` and retired at its last (ADR-0177 D7).
type ClassStore<F, const C: usize> = CappedSlab<TaskCell<F>, u64, Retire, C>;

/// A class's slab with its spare and the kept lend, behind this module's two doors. The spare is
/// one Live slot holding the husk, its header Free and on no list, its generation carried by no
/// `TaskId`; a class has at most one. The **kept lend** is the spare's occupant as the slab last
/// lent it, kept while nothing else borrows the slab: the slab, the spare and the lend are
/// private here, every other borrow of the slab, shared or mutable, passes [`ClassSlab::slab`]
/// or [`ClassSlab::slab_mut`], and each forgets the lend first. So a consecutive first poll
/// reaches its slot with no walk, and the lend is never used past a borrow that could invalidate
/// it (`SAFETY.md` §3).
mod class {
    use core::cell::Cell;
    use core::ptr::NonNull;

    use inf_foundation::bounded::{LookupMut, SlabReserved, SlotId};

    use super::{ClassStore, TaskCell};

    pub(super) struct ClassSlab<F, const C: usize> {
        slab: ClassStore<F, C>,
        spare: Option<SlotId<u64>>,
        lent: Cell<Option<NonNull<TaskCell<F>>>>,
    }

    /// The spare lent to one permit: its slot's index, the occupant where it lies, and the
    /// spare's handle, which the permit's task takes.
    pub(super) struct Lend<'a, F> {
        pub(super) index: u32,
        pub(super) cell: &'a mut TaskCell<F>,
        pub(super) spare: &'a mut Option<SlotId<u64>>,
    }

    /// The answer to [`ClassSlab::lend`]: the spare, or the slab's refusal, nothing taken.
    pub(super) enum Lent<'a, F> {
        Spare(Lend<'a, F>),
        Full,
        Exhausted,
    }

    impl<F, const C: usize> ClassSlab<F, C> {
        /// A built class slab, with no spare and nothing lent.
        pub(super) fn new(slab: ClassStore<F, C>) -> ClassSlab<F, C> {
            ClassSlab { slab, spare: None, lent: Cell::new(None) }
        }

        /// The door for a shared borrow of the slab: the lend is forgotten first, since a shared
        /// reborrow of a chunk would leave it valid for reads alone.
        #[inline(always)]
        pub(super) fn slab(&self) -> &ClassStore<F, C> {
            self.lent.set(None);
            &self.slab
        }

        /// The door for a mutable borrow of the slab: the lend is forgotten first.
        #[inline(always)]
        pub(super) fn slab_mut(&mut self) -> &mut ClassStore<F, C> {
            self.lent.set(None);
            &mut self.slab
        }

        /// Whether the class holds its spare, which its slab counts live and no task holds.
        #[inline]
        pub(super) fn has_spare(&self) -> bool {
            self.spare.is_some()
        }

        /// The spare, lent for one permit: through the kept lend, with no slab call; else by
        /// `get_mut` of the spare's handle; else, with no spare, the slab's `reserve` and
        /// `publish_mut` of the husk, whose handle becomes the spare (counted and reported as
        /// any publish). Each of the last two keeps its lend. `Full` and `Exhausted` take
        /// nothing.
        #[inline(always)]
        pub(super) fn lend(&mut self) -> Lent<'_, F> {
            let (index, lent) = match (self.spare, self.lent.get()) {
                (Some(spare), Some(kept)) => (spare.index(), kept),
                (Some(spare), None) => match self.slab_mut().get_mut(spare) {
                    LookupMut::Live(cell) => (spare.index(), NonNull::from(cell)),
                    LookupMut::Stale | LookupMut::Vacant => {
                        unreachable!("a class's spare is a live slot of its slab")
                    }
                },
                (None, _) => match self.slab_mut().reserve() {
                    SlabReserved::Slot(reservation) => {
                        let (spare, cell) = reservation.publish_mut(TaskCell::Done);
                        let lent = NonNull::from(cell);
                        self.spare = Some(spare);
                        (spare.index(), lent)
                    }
                    SlabReserved::Full => return Lent::Full,
                    SlabReserved::Exhausted => return Lent::Exhausted,
                },
            };
            self.lent.set(Some(lent));
            // SAFETY: `lent` is the spare's occupant as the slab lent it, by this call's
            // `get_mut` or `publish_mut` or by an earlier one's: no borrow of the slab, shared
            // or mutable, has been made since, for every one passes `slab` or `slab_mut`, which
            // forget the lend, and this `&mut self` excludes any other. A Live slot lies where
            // it is until a take, a release or a retire step (ADR-0194 D1's notes), each of
            // which passes a door. The `&mut` lives for the permit's borrow of the executor.
            let cell = unsafe { &mut *lent.as_ptr() };
            Lent::Spare(Lend { index, cell, spare: &mut self.spare })
        }
    }
}

use class::{ClassSlab, Lend, Lent};

/// The chunk a class of future `F` is built at: `chunk_slots_for` over its slot's bytes, or 0
/// past one 64 KiB allocation, where no class is built.
const fn chunk_of<F>() -> usize {
    match chunk_slots_for(CappedSlab::<TaskCell<F>, u64, Retire>::SLOT_BYTES) {
        Some(slots) => slots,
        None => 0,
    }
}

/// The classes a cell's executor serves, each class slab's builder registered on the cell's
/// census when its class is, so a layout is its cell's own (ADR-0177 D2, ADR-0171 A2.1).
pub struct ExecutorLayout {
    census: Rc<CapCensus>,
    classes: Vec<Box<dyn ClassBuild>>,
}

/// Why an executor was not built: the class whose storage refused, and how.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ExecutorInitError {
    /// The refused class's census row (its cap's name), or the header array's.
    pub row: &'static str,
    /// The refusal.
    pub error: CapError,
}

impl core::fmt::Display for ExecutorInitError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "the executor's class {} was not built: {}", self.row, self.error)
    }
}

impl std::error::Error for ExecutorInitError {}

/// The header array's name in an [`ExecutorInitError`].
const HEADER_ARRAY_ROW: &str = "wake-headers";

impl ExecutorLayout {
    /// An empty layout on `census`, the cell's own.
    #[must_use]
    pub fn new(census: &Rc<CapCensus>) -> ExecutorLayout {
        ExecutorLayout { census: Rc::clone(census), classes: Vec::new() }
    }

    /// Registers a class of `cap` tasks of the future `factory` makes, its slab's builder on the
    /// layout's census, and returns its handle. The factory is read for its type and never
    /// called.
    ///
    /// # Errors
    /// `CapError::Layout` past [`TASK_CLASSES_MAX`] classes, or for a slot past 64 KiB; the
    /// builder's own refusal (`Layout`, `Census`), its row then registered.
    pub fn class<Args, Fac: TaskFactory<Args>>(
        &mut self,
        factory: &Fac,
        cap: Cap,
    ) -> Result<TaskClass, CapError> {
        let _ = factory;
        if self.classes.len() >= TASK_CLASSES_MAX {
            return Err(CapError::Layout);
        }
        let index = u32::try_from(self.classes.len()).map_err(|_| CapError::Layout)?;
        let (builder, instance) = class_builder::<Fac::Future>(cap, &self.census)?;
        self.classes.push(builder);
        Ok(TaskClass { index, instance })
    }

    /// The slot bytes of a class of the future `factory` makes, and the slots one chunk of its
    /// slab holds: `chunk_slots_for` over the slot (ADR-0194 A1), 0 past 64 KiB, where `class`
    /// refuses it. Read from the factory's type alone; nothing is registered.
    #[must_use]
    pub fn slot_shape<Args, Fac: TaskFactory<Args>>(factory: &Fac) -> (usize, usize) {
        let _ = factory;
        (CappedSlab::<TaskCell<Fac::Future>, u64, Retire>::SLOT_BYTES, chunk_of::<Fac::Future>())
    }

    /// The executor of this layout's classes, built before its cell serves: each class slab by
    /// its builder's steps, one chunk or directory a step, then the header array.
    ///
    /// # Errors
    /// [`ExecutorInitError`] naming the class whose storage refused, or the header array.
    pub fn build(self) -> Result<CellExecutor, ExecutorInitError> {
        let ExecutorLayout { census, classes: builders } = self;
        drop(census);
        let mut classes: Vec<ClassEntry> = Vec::with_capacity(builders.len());
        let mut headers: u32 = 0;
        for builder in builders {
            let (row, cap, instance) = (builder.row(), builder.cap(), builder.instance());
            let base = headers;
            headers = headers
                .checked_add(cap)
                .ok_or(ExecutorInitError { row: HEADER_ARRAY_ROW, error: CapError::Layout })?;
            let (object, slab_type) =
                builder.build().map_err(|error| ExecutorInitError { row, error })?;
            classes.push(ClassEntry { object, slab_type, instance, base, retired: 0 });
        }
        let mark = ThreadMark::claim();
        let array = HeaderArray::build(&classes, headers, mark)?;
        Ok(CellExecutor { classes, array, mark, not_send: PhantomData })
    }
}

/// A class slab's builder, erased: the layout keeps one a class until `build`.
trait ClassBuild {
    fn row(&self) -> &'static str;
    fn cap(&self) -> u32;
    fn instance(&self) -> SlabInstance;
    /// The slab, built step by step, as its class's object with the `TypeId` of the object's
    /// concrete type, `ClassSlab<F, C>`, beside it: one call answers both, so no code pairs an
    /// object with another's id (`class_slab`'s cast). A refused step retires what was built, by
    /// steps.
    fn build(self: Box<Self>) -> Result<(Box<dyn ClassObject>, TypeId), CapError>;
}

/// The builder of a class of future `F` at chunk `C`.
struct ClassBuilder<F, const C: usize> {
    builder: SlabBuilder<TaskCell<F>, u64, Retire, C>,
    cap: Cap,
}

/// A class of future `F`'s builder at the chunk its slot takes (ADR-0194 A1: a `match const` over
/// the seven powers of two, so no access pays a branch) and its slab's identity.
fn class_builder<F: Future<Output = ()> + 'static>(
    cap: Cap,
    census: &Rc<CapCensus>,
) -> Result<(Box<dyn ClassBuild>, SlabInstance), CapError> {
    match const { chunk_of::<F>() } {
        64 => builder_at::<F, 64>(cap, census),
        32 => builder_at::<F, 32>(cap, census),
        16 => builder_at::<F, 16>(cap, census),
        8 => builder_at::<F, 8>(cap, census),
        4 => builder_at::<F, 4>(cap, census),
        2 => builder_at::<F, 2>(cap, census),
        1 => builder_at::<F, 1>(cap, census),
        _ => Err(CapError::Layout),
    }
}

fn builder_at<F: Future<Output = ()> + 'static, const C: usize>(
    cap: Cap,
    census: &Rc<CapCensus>,
) -> Result<(Box<dyn ClassBuild>, SlabInstance), CapError> {
    let builder = ClassStore::<F, C>::builder(cap, census)?;
    let instance = builder.instance();
    Ok((Box::new(ClassBuilder { builder, cap }), instance))
}

impl<F: Future<Output = ()> + 'static, const C: usize> ClassBuild for ClassBuilder<F, C> {
    fn row(&self) -> &'static str {
        self.cap.name()
    }

    fn cap(&self) -> u32 {
        self.cap.entries_max()
    }

    fn instance(&self) -> SlabInstance {
        self.builder.instance()
    }

    fn build(self: Box<Self>) -> Result<(Box<dyn ClassObject>, TypeId), CapError> {
        let mut builder = self.builder;
        loop {
            match builder.step() {
                Built::More(next) => builder = next,
                Built::Live(slab) => {
                    let object: Box<dyn ClassObject> = Box::new(ClassSlab::new(slab));
                    return Ok((object, TypeId::of::<ClassSlab<F, C>>()));
                }
                Built::Refused(mut retiring, refused) => loop {
                    match retiring.step() {
                        Retiring::More(next) => retiring = next,
                        Retiring::Done => return Err(refused),
                    }
                },
            }
        }
    }
}

/// A live class, erased: a poll from the ready list is one indirect call, as a type-erased
/// task's always was. Its every method reaches the slab through a door of [`class`].
trait ClassObject {
    /// Polls the occupant `slot` names once, in place; Ready, `header` is marked Finished and
    /// the future dropped in place.
    ///
    /// # Safety
    /// `header` is `slot`'s own, open, on the owner thread, and the occupant's future is live.
    unsafe fn poll(&mut self, slot: SlotId<u64>, header: Header, cx: &mut Context<'_>) -> Poll<()>;
    /// Drops the occupant's live future in place: the executor's drop.
    fn drop_future(&mut self, slot: SlotId<u64>);
    /// Frees a finished occupant's slot: its generation advances, or it retires.
    fn reclaim(&mut self, slot: SlotId<u64>);
    /// Whether `slot` names this class's current occupant.
    fn holds(&self, slot: SlotId<u64>) -> bool;
    /// Slots its tasks hold, Active and Retired: the spare is none of them.
    fn len(&self) -> u32;
    /// KE27's plant: before a task's activation its cell moves to another slot of its class
    /// (`reserve`, `publish` of the husk, `take`, a write through `get_mut`) and its header is
    /// re-pointed there, the move the in-place rule rules out. Sound only for an `Unpin`
    /// future, so its one row runs a test of one; answers the slot the task is in.
    #[cfg(inf_canary_first_poll_off_slot)]
    fn move_off_slot(&mut self, header: Header, slot: SlotId<u64>) -> SlotId<u64>;
}

impl<F: Future<Output = ()> + 'static, const C: usize> ClassSlab<F, C> {
    /// The live occupant `slot` names: a header names its slot only while it holds it.
    fn occupant(&mut self, slot: SlotId<u64>) -> &mut TaskCell<F> {
        match self.slab_mut().get_mut(slot) {
            LookupMut::Live(cell) => cell,
            LookupMut::Stale | LookupMut::Vacant => {
                unreachable!("a header names its slot only while the slot holds its task")
            }
        }
    }
}

impl<F: Future<Output = ()> + 'static, const C: usize> ClassObject for ClassSlab<F, C> {
    unsafe fn poll(&mut self, slot: SlotId<u64>, header: Header, cx: &mut Context<'_>) -> Poll<()> {
        // SAFETY: per this method's contract.
        unsafe { poll_in_place(self.occupant(slot), header, cx) }
    }

    fn drop_future(&mut self, slot: SlotId<u64>) {
        let cell = self.occupant(slot);
        let TaskCell::Live(future) = cell else {
            unreachable!("a live header's slot holds its future")
        };
        // SAFETY: the header's state says the future is live; it is dropped once, here, and the
        // cell is its husk before anything reads it again.
        unsafe { ManuallyDrop::drop(future.get_mut()) };
        *cell = TaskCell::Done;
    }

    fn reclaim(&mut self, slot: SlotId<u64>) {
        // The husk is released where it lies: its future was dropped in place, and `TaskCell`
        // has no drop glue, so nothing moves and nothing drops.
        match self.slab_mut().release(slot) {
            Released::Dropped => {}
            Released::Stale | Released::Vacant => {
                unreachable!("a finished header names its slot until the slot is reclaimed")
            }
        }
    }

    fn holds(&self, slot: SlotId<u64>) -> bool {
        matches!(self.slab().get(slot), Lookup::Live(_))
    }

    fn len(&self) -> u32 {
        // The spare is live in the slab and held by no task: at most one, and live when set.
        self.slab().len() - u32::from(self.has_spare())
    }

    #[cfg(inf_canary_first_poll_off_slot)]
    fn move_off_slot(&mut self, header: Header, slot: SlotId<u64>) -> SlotId<u64> {
        use inf_foundation::bounded::{SlabReserved, Taken};
        let slab = self.slab_mut();
        let SlabReserved::Slot(reservation) = slab.reserve() else { return slot };
        let moved = reservation.publish(TaskCell::Done);
        let Taken::Value(cell) = slab.take(slot) else { return slot };
        if let LookupMut::Live(at) = slab.get_mut(moved) {
            *at = cell;
        }
        // SAFETY: the slot's own header: open, on this thread.
        unsafe { owner(header) }.slot.set(Some(moved));
        moved
    }
}

/// One poll of a slot's future in place. Ready, its header is marked Finished and then the
/// future is dropped in place, so a waker the future's drop releases finds its task finished and
/// undecided, and settles nothing.
///
/// # Safety
/// `header` is the slot's own, open, on the owner thread, and `cell`'s future is live.
#[inline(always)]
unsafe fn poll_in_place<F: Future<Output = ()>>(
    cell: &mut TaskCell<F>,
    header: Header,
    cx: &mut Context<'_>,
) -> Poll<()> {
    let TaskCell::Live(future) = cell else {
        unreachable!("a live header's slot holds its future")
    };
    // SAFETY: the future was placed in its slot before its first poll, and a slab never moves a
    // value it holds (its chunks are fixed allocations): the slot is its address until it is
    // dropped in place below; it is live, per the contract.
    let pinned = unsafe { Pin::new_unchecked(&mut **future.get_mut()) };
    let poll = pinned.poll(cx);
    if poll.is_ready() {
        // SAFETY: per the contract.
        unsafe { owner(header) }.state.set(TaskState::Finished);
        // SAFETY: the future is live and finished; dropped in place once, here.
        unsafe { ManuallyDrop::drop(future.get_mut()) };
        *cell = TaskCell::Done;
    }
    poll
}

// ---- The header array ---------------------------------------------------------------------------

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum TaskState {
    /// No occupant: the header's slot is free.
    Free,
    /// Suspended, waiting for a wake.
    Idle,
    /// On the ready list, to be polled.
    Queued,
    /// Being polled right now.
    Running,
    /// Woken while being polled — requeue on `Pending`.
    RunningWoken,
    /// Its future returned Ready: it is dropped in place, then the task is decided Free or
    /// Retired.
    Finished,
    /// Finished while a waker names the header or a foreign wake for it is queued: the slot stays
    /// held until that waker's drop links a reclaim entry. Wakes are no-ops.
    Retired,
    /// Retired and no longer named: on the ready list for the reclaim that frees its slot.
    Reclaiming,
    /// The executor is dropping and the task is live: its future is dropped next. To a waker,
    /// as Closing.
    Dropping,
    /// The executor is dropping: a waker dropped now changes only its count.
    Closing,
}

/// The front of the header array's one allocation: the remote stack first, so a bridge's stack
/// is the array's address, then the ready list's ends, the headers' count and the allocation's
/// layout.
#[repr(C)]
struct ArrayHead {
    stack: RemoteStack,
    ready_head: Cell<u32>,
    ready_tail: Cell<u32>,
    /// Headers after the head. It and `layout` are written at the build and never again: the
    /// thread that frees the array reads both.
    len: u32,
    layout: Layout,
}

/// A task's wake header (`repr(C)`): the bridge first, so a waker's data pointer is the bridge's
/// address too; then its place in its array, written at the build and never again, so any thread
/// reaches the array from it; then the owner part, reached only past the lane compare.
#[repr(C)]
struct WakeHeader {
    bridge: WakeBridge,
    /// The occupant's handle while the header's slot holds a task.
    slot: Cell<Option<SlotId<u64>>>,
    /// `o`, the wakers the owner lane counted: the header's live wakers are `o + c`.
    owner_count: Cell<i64>,
    index: u32,
    /// The class whose slab the header's slot is in, fixed at the build.
    class: u32,
    ready_next: Cell<u32>,
    state: Cell<TaskState>,
}

/// A wake header's address, with the provenance of its array's whole allocation.
type Header = NonNull<WakeHeader>;

// The bridge's four words, the occupant's handle (two words), `o`, and four narrow fields in the
// ninth: nine words, as S01's header with its own allocation (ADR-0003 A1.2.2).
const _: () = assert!(size_of::<WakeHeader>() == 72, "a wake header is nine words");

// The parts any thread reaches through a waker: atomics only, `Sync` by their fields.
const _: () = {
    const fn sync<T: Sync>() {}
    sync::<WakeBridge>();
    sync::<RemoteStack>();
};

/// Where the headers start in the array's allocation: past the head, at the header's alignment.
const HEADERS_OFFSET: usize =
    size_of::<ArrayHead>().div_ceil(align_of::<WakeHeader>()) * align_of::<WakeHeader>();

/// The array's allocation for `len` headers; `None` past what a `Layout` holds.
fn array_layout(len: usize) -> Option<Layout> {
    let headers = size_of::<WakeHeader>().checked_mul(len)?;
    let size = HEADERS_OFFSET.checked_add(headers)?;
    let align = align_of::<ArrayHead>().max(align_of::<WakeHeader>());
    Layout::from_size_align(size, align).ok()
}

/// The executor's header array.
struct HeaderArray;

impl HeaderArray {
    /// One allocation: the stack, counted once (the executor's own count), the empty ready list,
    /// and `len` open headers, each class's from its base, each Free with no waker counted.
    fn build(
        classes: &[ClassEntry],
        len: u32,
        mark: ThreadMark,
    ) -> Result<NonNull<ArrayHead>, ExecutorInitError> {
        let refused = |error| ExecutorInitError { row: HEADER_ARRAY_ROW, error };
        let layout = array_layout(len as usize).ok_or(refused(CapError::Layout))?;
        // SAFETY: the layout's size is at least the head's, never zero.
        let raw = unsafe { alloc(layout) };
        let array = NonNull::new(raw).ok_or(refused(CapError::Alloc))?.cast::<ArrayHead>();
        // SAFETY: a fresh allocation of `layout`, aligned for the head; written whole here before
        // any reference to it is formed.
        unsafe {
            array.write(ArrayHead {
                stack: RemoteStack::new(),
                ready_head: Cell::new(NO_HEADER),
                ready_tail: Cell::new(NO_HEADER),
                len,
                layout,
            });
        }
        // SAFETY: the head was written above; the stack stays at this address for the array's life.
        let stack = unsafe { &(*array.as_ptr()).stack };
        let mut class: u32 = 0;
        for index in 0..len {
            while let Some(next) = classes.get(class as usize + 1)
                && next.base <= index
            {
                class += 1;
            }
            // SAFETY: `index < len`, so the header lies inside the allocation; written once here.
            unsafe {
                header_at(array, index).write(WakeHeader {
                    bridge: WakeBridge::new(stack, mark),
                    slot: Cell::new(None),
                    owner_count: Cell::new(0),
                    index,
                    class,
                    ready_next: Cell::new(NO_HEADER),
                    state: Cell::new(TaskState::Free),
                });
            }
        }
        Ok(array)
    }
}

/// Header `index` of `array`.
///
/// # Safety
/// `array` is live and `index` is below its `len`.
#[inline(always)]
unsafe fn header_at(array: NonNull<ArrayHead>, index: u32) -> Header {
    // SAFETY: per the contract the offset lies inside the allocation, and `array` carries the
    // provenance of all of it.
    unsafe { array.byte_add(HEADERS_OFFSET).cast::<WakeHeader>().add(index as usize) }
}

/// The array a header lives in, with the provenance of its whole allocation.
///
/// # Safety
/// `header` is a live header's address: a waker names it, a push keeps it, or the executor holds
/// it.
#[inline(always)]
unsafe fn array_of(header: Header) -> NonNull<ArrayHead> {
    // SAFETY: `index`, written at the build and never again, is readable on any thread that holds
    // the header; the header lies `index` headers past the array's start.
    unsafe {
        let index = (*header.as_ptr()).index as usize;
        header.cast::<u8>().byte_sub(HEADERS_OFFSET + index * size_of::<WakeHeader>()).cast()
    }
}

/// The owner part of a header.
///
/// # Safety
/// `header` is live and open, and the caller is the owner thread.
#[inline(always)]
unsafe fn owner<'a>(header: Header) -> &'a WakeHeader {
    // SAFETY: per the caller's contract; the owner-part fields are `Cell`s only the owner thread
    // touches, and the bridge is atomics.
    unsafe { header.as_ref() }
}

/// The bridge of a header.
///
/// # Safety
/// `header` is live: a waker names it, a push keeps it, or the executor holds it.
unsafe fn bridge<'a>(header: Header) -> &'a WakeBridge {
    // SAFETY: per the caller's contract; the bridge is atomics only, so any thread may read it.
    unsafe { &(*header.as_ptr()).bridge }
}

/// Links `header` at the ready list's tail: a wake of an Idle task, a requeue, or a reclaim.
///
/// # Safety
/// `header` is open, on the owner thread, and on no list.
#[inline(always)]
unsafe fn link_ready(header: Header) {
    // SAFETY: per the contract; the array outlives its open headers.
    unsafe {
        let array = array_of(header);
        let head = array.as_ref();
        let index = owner(header).index;
        owner(header).ready_next.set(NO_HEADER);
        match head.ready_tail.get() {
            NO_HEADER => head.ready_head.set(index),
            tail => owner(header_at(array, tail)).ready_next.set(index),
        }
        head.ready_tail.set(index);
    }
}

/// The ready list's head, unlinked.
///
/// # Safety
/// `array` is live, on the owner thread.
#[inline(always)]
unsafe fn pop_ready(array: NonNull<ArrayHead>) -> Option<Header> {
    // SAFETY: per the contract; a linked index is below the array's `len`.
    unsafe {
        let head = array.as_ref();
        let index = head.ready_head.get();
        if index == NO_HEADER {
            return None;
        }
        let header = header_at(array, index);
        let next = owner(header).ready_next.get();
        head.ready_head.set(next);
        if next == NO_HEADER {
            head.ready_tail.set(NO_HEADER);
        }
        Some(header)
    }
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
    let owned =
        (marked == ThreadMark::current() || PLANT_OWNER_UNCHECKED) && !PLANT_FOREIGN_LANE_ALWAYS;
    // SAFETY: `data` is a header's own pointer (`waker_ref`, `waker_clone`), never null.
    owned.then(|| unsafe { NonNull::new_unchecked(data.cast_mut().cast::<WakeHeader>()) })
}

/// The owner lane's wake transition, a single-threaded waker's: Idle → Queued and linked;
/// Running → RunningWoken; otherwise nothing. Runs inside `Waker::wake`, so it touches only the
/// header array, never the executor.
///
/// # Safety
/// `header` is open and the caller is the owner thread.
#[inline(always)]
unsafe fn wake_task(header: Header) {
    // SAFETY: per the contract.
    let owner = unsafe { owner(header) };
    match owner.state.get() {
        TaskState::Idle => {
            owner.state.set(TaskState::Queued);
            // SAFETY: an Idle header is on no list.
            unsafe { link_ready(header) };
        }
        // Woken during its own poll (wake-before-suspend): remember it so
        // the executor requeues on `Pending` — the classic lost-wakeup hole.
        TaskState::Running => owner.state.set(TaskState::RunningWoken),
        // Already queued / already flagged / finished: wakes coalesce.
        TaskState::Queued
        | TaskState::RunningWoken
        | TaskState::Free
        | TaskState::Finished
        | TaskState::Retired
        | TaskState::Reclaiming
        | TaskState::Dropping
        | TaskState::Closing => {}
    }
}

/// The owner lane's clone: `o + 1`, compared with the bound first (ADR-0003 A1.2.6).
#[inline(always)]
fn owner_clone(owner: &WakeHeader) {
    let count = owner.owner_count.get();
    if !PLANT_COUNT_UNBOUNDED && count >= LANE_COUNT_MAX {
        // Fail-stop before the count is written, as `Rc` and `Arc` at their bounds.
        std::process::abort();
    }
    owner.owner_count.set(count + 1);
}

/// The owner lane's drop: `o − 1`, and on a Retired header its settle, which links its reclaim
/// when no waker names it and it is not queued (ADR-0003 A1.2.4).
///
/// # Safety
/// `header` came from the lane compare.
#[inline(always)]
unsafe fn owner_drop(header: Header) {
    // SAFETY: the compare matched, so this is the owner thread and the header is open.
    let owner = unsafe { owner(header) };
    owner.owner_count.set(owner.owner_count.get() - 1);
    let settles = match owner.state.get() {
        TaskState::Retired => true,
        TaskState::Finished => PLANT_SETTLE_ON_DEAD,
        TaskState::Free
        | TaskState::Idle
        | TaskState::Queued
        | TaskState::Running
        | TaskState::RunningWoken
        | TaskState::Reclaiming
        | TaskState::Dropping
        | TaskState::Closing => false,
    };
    if settles && !PLANT_SETTLE_HELD_SKIPPED {
        // SAFETY: as above.
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
            unsafe { wake_task(header) };
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
        Some(header) => unsafe { wake_task(header) },
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
    // never took. The header outlives the poll: the executor holds its array.
    ManuallyDrop::new(unsafe { Waker::from_raw(raw) })
}

/// The foreign lane (ADR-0003 A1.2.5, A1.6): the four entries the vtable's arms call when the
/// lane compare fails, and `settle_held`, the owner lane's settle of a Retired header. Each is
/// `#[inline(never)]`, so its symbol bounds what the waker gate scans as the owner lane: these
/// five bodies are its boundary, and every atomic of the waker path is in them or in
/// `inf_foundation::wake`, which they call. `executor::foreign` passes the bridge only its own
/// remote stack (`WakeBridge::stack`) and, as its push's node, the header's own address.
mod foreign {
    use core::ptr::NonNull;

    use inf_foundation::wake::{Released, RemoteStack, WakeBridge};

    use super::{Header, TaskState, WakeHeader};

    /// The bridge at `data` and the remote stack it queues on.
    ///
    /// # Safety
    /// `data` is a live header's address, which the caller's waker names: it keeps the header
    /// and, through the stack's count of a closed header, the array and its stack allocated
    /// (ADR-0003 A1.3).
    #[inline(always)]
    unsafe fn bridge_and_stack<'a>(data: *const ()) -> (&'a WakeBridge, &'a RemoteStack) {
        // SAFETY: per the caller's contract; both are atomics only, `Sync` by their fields.
        unsafe {
            let bridge = &*data.cast::<WakeBridge>();
            (bridge, &*bridge.stack())
        }
    }

    /// The node a push links: the header's own address, whose provenance reaches the whole
    /// array, which the drain needs (a reference to the bridge reaches its 32 bytes alone).
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

    /// A foreign `wake`, which consumes its waker; releases a closed header it leaves unnamed.
    ///
    /// # Safety
    /// `data` is a live header's address, which the caller's waker names and gives up here.
    #[inline(never)]
    pub(super) unsafe fn wake(data: *const ()) {
        // SAFETY: per this function's contract, the waker keeps the header and the stack
        // allocated until its swap, or its push's compare-and-swap, inside `foreign_wake_at`;
        // after that access the drain, or another thread's last drop, may free both while these
        // references and the callee's `&self` are live. They cover atomics only, as `Arc::drop`'s
        // `&AtomicUsize` does across its decrement, and after that access neither this call nor
        // the callee reads through or copies one unless it answered `Last`, which makes this
        // call the one releaser (ADR-0003 A1.2.5); `tests/waker_race.rs` races that window under
        // Miri.
        let (bridge, stack) = unsafe { bridge_and_stack(data) };
        // SAFETY: per this function's contract.
        let released = bridge.foreign_wake_at(stack, unsafe { node(data) });
        // SAFETY: `Last` makes this call the header's releaser; nothing above touches it again.
        unsafe { settle_released(data, released) };
    }

    /// A foreign drop; releases a closed header it leaves unnamed.
    ///
    /// # Safety
    /// `data` is a live header's address, which the caller's waker names and gives up here.
    #[inline(never)]
    pub(super) unsafe fn drop(data: *const ()) {
        // SAFETY: per this function's contract, the waker keeps the header and the stack
        // allocated until its swap, or its push's compare-and-swap, inside `foreign_drop_at`;
        // after that access the drain, or another thread's last drop, may free both while these
        // references and the callee's `&self` are live. They cover atomics only, as `Arc::drop`'s
        // `&AtomicUsize` does across its decrement, and after that access neither this call nor
        // the callee reads through or copies one unless it answered `Last`, which makes this
        // call the one releaser (ADR-0003 A1.2.5); `tests/waker_race.rs` races that window under
        // Miri.
        let (bridge, stack) = unsafe { bridge_and_stack(data) };
        // SAFETY: per this function's contract.
        let released = bridge.foreign_drop_at(stack, unsafe { node(data) });
        // SAFETY: `Last` makes this call the header's releaser; nothing above touches it again.
        unsafe { settle_released(data, released) };
    }

    /// # Safety
    /// `released` is the header's own swap's answer; on `Last` this thread is its one releaser.
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

    /// The owner lane's settle of a Retired header (ADR-0003 A1.3): one `Relaxed` load of the
    /// lane word, and its reclaim entry linked on the ready list when no waker names it and it is
    /// not queued, so the next `run_ready` frees its slot. Runs inside a waker's drop, during
    /// another task's poll or none, so it reaches the ready list through the header's array,
    /// never through the executor.
    ///
    /// # Safety
    /// `header` is an open header on the owner thread whose waker was just dropped.
    #[inline(never)]
    pub(super) unsafe fn settle_held(header: Header) {
        // SAFETY: per this function's contract.
        let owner = unsafe { super::owner(header) };
        // SAFETY: as above.
        if !unsafe { super::bridge(header) }.quiet(owner.owner_count.get()) {
            return;
        }
        owner.state.set(TaskState::Reclaiming);
        // SAFETY: a Retired (or Finished) header is on no list; open, on the owner thread.
        unsafe { super::link_ready(header) };
    }
}

/// Releases a closed header's count of its array: the array and its stack are freed with the
/// last count (ADR-0003 A1.3).
///
/// # Safety
/// `header` is closed and this thread's swap (or the executor's close or last take) left it at
/// `c = 0` with `Q` clear: no waker names it, no push holds it, and this thread is its one
/// releaser.
unsafe fn free_closed(header: Header) {
    if PLANT_CLOSED_HEADER_KEPT {
        return;
    }
    // SAFETY: per this function's contract the array lives until this count is released.
    unsafe { release_array(array_of(header)) };
}

/// Releases one count of the array's stack, and frees the array with the last.
///
/// # Safety
/// `array` is live and the caller holds one of its stack's counts, which this call gives up.
unsafe fn release_array(array: NonNull<ArrayHead>) {
    // SAFETY: the caller's count keeps the array allocated across the release.
    match unsafe { array.as_ref() }.stack.release() {
        // SAFETY: the count reached zero: no header is named and no executor holds the array,
        // so this thread frees it: its headers and head dropped in place, then the allocation,
        // by the layout the build allocated it with (written then, never again).
        Freed::Stack => unsafe {
            let (len, layout) = (array.as_ref().len, array.as_ref().layout);
            for index in 0..len {
                core::ptr::drop_in_place(header_at(array, index).as_ptr());
            }
            core::ptr::drop_in_place(array.as_ptr());
            dealloc(array.as_ptr().cast::<u8>(), layout);
        },
        Freed::No => {}
    }
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
    #[cfg(not(inf_canary_ready_ignores_queued))]
    let quiet = unsafe { bridge(header) }.quiet(owner_count);
    // Canary: a finished header's slot frees with `Q` set.
    // SAFETY: as above.
    #[cfg(inf_canary_ready_ignores_queued)]
    let quiet = unsafe { bridge(header) }.unnamed_ignoring_queue(owner_count);
    quiet
}

// ---- The executor ----------------------------------------------------------

/// Single-threaded task executor for one cell. See module docs.
///
/// It stays on the thread that built it, whose mark its headers carry:
///
/// ```compile_fail,E0277
/// fn on_another_thread<T: Send>(_: T) {}
/// let (census, _mark) = inf_foundation::bounded::CapCensus::new(inf_foundation::CellId(0));
/// let executor = inf_runtime::ExecutorLayout::new(&census).build().expect("no class refuses");
/// on_another_thread(executor);
/// ```
pub struct CellExecutor {
    /// The layout's classes, indexed by [`TaskClass`]'s index.
    classes: Vec<ClassEntry>,
    /// The header array, holding the remote stack: the executor holds one count of it until its
    /// drop's last release.
    array: NonNull<ArrayHead>,
    /// This thread's mark, which every header of this executor carries.
    mark: ThreadMark,
    /// `!Send` and `!Sync` by itself: the thread that built the executor is the one that polls
    /// it, so its mark is the owner lane's (ADR-0003 A1.2.3).
    not_send: PhantomData<*const ()>,
}

/// A live class: its slab behind its object, the `TypeId` of the object's concrete type (written
/// with the object by `ClassBuild::build` and never after), the slab's identity, its first header
/// in the array and its Retired count.
struct ClassEntry {
    object: Box<dyn ClassObject>,
    slab_type: TypeId,
    instance: SlabInstance,
    base: u32,
    /// Retired and Reclaiming headers: finished tasks whose slots stay held.
    retired: u32,
}

impl CellExecutor {
    fn stack_ref(&self) -> &RemoteStack {
        // SAFETY: the executor holds a count of its array until its drop's last release.
        unsafe { &self.array.as_ref().stack }
    }

    /// A permit for one task of `class`, whose future type the permit's use fixes: `Foreign`
    /// for another executor's class or another future type, `Full` when every slot of the class
    /// holds a task (the class's census row counts it), `Exhausted` when every slot retired.
    /// The permit is the class's spare, made from a free slot when the class has none; nothing
    /// else is taken until the permit makes the task.
    #[inline(always)]
    pub fn reserve<F: Future<Output = ()> + 'static>(
        &mut self,
        class: TaskClass,
    ) -> TaskAdmission<'_, F> {
        match const { chunk_of::<F>() } {
            64 => self.reserve_at::<F, 64>(class),
            32 => self.reserve_at::<F, 32>(class),
            16 => self.reserve_at::<F, 16>(class),
            8 => self.reserve_at::<F, 8>(class),
            4 => self.reserve_at::<F, 4>(class),
            2 => self.reserve_at::<F, 2>(class),
            1 => self.reserve_at::<F, 1>(class),
            _ => TaskAdmission::Foreign,
        }
    }

    #[inline(always)]
    fn reserve_at<F: Future<Output = ()> + 'static, const C: usize>(
        &mut self,
        class: TaskClass,
    ) -> TaskAdmission<'_, F> {
        let CellExecutor { classes, array, .. } = self;
        let Some(entry) = classes.get_mut(class.index as usize) else {
            return TaskAdmission::Foreign;
        };
        let ClassEntry { object, slab_type, instance, base, retired, .. } = entry;
        // Another executor's class first, by its slab's identity, then another future type.
        if !PLANT_INSTANCE_BLIND && *instance != class.instance {
            return TaskAdmission::Foreign;
        }
        let Some(slab) = class_slab::<F, C>(*slab_type, object) else {
            return TaskAdmission::Foreign;
        };
        // The slab's own admission decides a class with no spare, so its census row counts a
        // crossing and its canaries reach the class.
        let Lend { index, cell, spare } = match slab.lend() {
            Lent::Spare(lend) => lend,
            Lent::Full => return TaskAdmission::Full,
            Lent::Exhausted => return TaskAdmission::Exhausted,
        };
        // SAFETY: the spare's index is below its class's cap, so its header lies in the array.
        let header = unsafe { header_at(*array, *base + index) };
        TaskAdmission::Granted(TaskPermit { cell, spare, retired, header, class: class.index })
    }

    /// Moves an Idle task to the ready list's tail (ADR-0177 D2): its owner queued work for it.
    /// A queued or running task's wakes coalesce; a stale or vacant id changes nothing.
    pub fn wake(&mut self, id: TaskId) -> Woken {
        let Some(header) = self.header_of(id) else { return Woken::Stale };
        // SAFETY: the executor holds its headers: open, on this thread.
        let state = unsafe { owner(header) }.state.get();
        match state {
            TaskState::Idle => {
                // SAFETY: as above.
                unsafe { wake_task(header) };
                Woken::Queued
            }
            TaskState::Running => {
                // SAFETY: as above.
                unsafe { wake_task(header) };
                Woken::AlreadyRunnable
            }
            TaskState::Queued | TaskState::RunningWoken => Woken::AlreadyRunnable,
            TaskState::Free
            | TaskState::Finished
            | TaskState::Retired
            | TaskState::Reclaiming
            | TaskState::Dropping
            | TaskState::Closing => Woken::Stale,
        }
    }

    /// Slots of `class` held: its tasks alive and its Retired ones (`conn-tasks`' count beside
    /// the connection slots it serves). Zero for a handle this executor did not make.
    #[must_use]
    pub fn class_tasks(&self, class: TaskClass) -> u32 {
        self.entry(class).map_or(0, |entry| entry.object.len())
    }

    /// Finished tasks of `class` whose slots stay held: Retired while a waker names the header
    /// or a foreign wake for it is queued, and until the `run_ready` that takes its reclaim.
    /// Zero for a handle this executor did not make.
    #[must_use]
    pub fn class_retired(&self, class: TaskClass) -> u32 {
        self.entry(class).map_or(0, |entry| entry.retired)
    }

    /// The header of the task `id` names, while its task holds its slot: the class slab's
    /// lookup of the handle (its instance, index and generation), then the header at its index.
    fn header_of(&self, id: TaskId) -> Option<Header> {
        let entry = self.classes.get(id.class as usize)?;
        if !entry.object.holds(id.slot) {
            return None;
        }
        // SAFETY: the slab holds the slot, so its index is below its class's cap and the header
        // lies in the class's run of the array.
        Some(unsafe { header_at(self.array, entry.base + id.slot.index()) })
    }

    /// The class `class` names, if this executor made it.
    fn entry(&self, class: TaskClass) -> Option<&ClassEntry> {
        let entry = self.classes.get(class.index as usize)?;
        (entry.instance == class.instance).then_some(entry)
    }

    /// Poll up to `budget` ready entries, each one activation or one reclaim. Returns how many
    /// were taken — the loop's EXECUTE step charges this against the foreground deficit. It
    /// first delivers the foreign wakes queued on the remote stack.
    pub fn run_ready(&mut self, budget: usize) -> usize {
        self.drain_remote();
        let mut polled = 0;
        while polled < budget {
            // SAFETY: the executor holds its array, on this thread.
            let Some(header) = (unsafe { pop_ready(self.array) }) else { break };
            polled += 1;
            // SAFETY: the executor holds its headers: open, on this thread.
            let state = unsafe { owner(header) }.state.get();
            match state {
                TaskState::Queued => self.poll_queued(header),
                TaskState::Reclaiming => self.reclaim(header),
                other @ (TaskState::Free
                | TaskState::Idle
                | TaskState::Running
                | TaskState::RunningWoken
                | TaskState::Finished
                | TaskState::Retired
                | TaskState::Dropping
                | TaskState::Closing) => {
                    unreachable!(
                        "the ready list links Queued and Reclaiming headers, not {other:?}"
                    )
                }
            }
        }
        polled
    }

    /// One activation of a queued task, through its class's erased object.
    fn poll_queued(&mut self, header: Header) {
        // SAFETY: the executor holds its headers: open, on this thread.
        let owner = unsafe { owner(header) };
        let Some(slot) = owner.slot.get() else {
            unreachable!("a queued header names its task's slot")
        };
        owner.state.set(TaskState::Running);
        let waker = waker_ref(header);
        let mut cx = Context::from_waker(&waker);
        let entry = &mut self.classes[owner.class as usize];
        #[cfg(inf_canary_first_poll_off_slot)]
        let slot = entry.object.move_off_slot(header, slot);
        // SAFETY: as above; a queued header's future is live.
        match unsafe { entry.object.poll(slot, header, &mut cx) } {
            Poll::Ready(()) => {
                // SAFETY: as above; its future was dropped in place.
                if unsafe { finished(header, &mut entry.retired) } {
                    entry.object.reclaim(slot);
                }
            }
            // SAFETY: as above.
            Poll::Pending => unsafe { suspended(header) },
        }
    }

    /// A Retired header no waker names any more: its slot is released, the husk dropped where it
    /// lies, and its generation advances.
    fn reclaim(&mut self, header: Header) {
        // SAFETY: the executor holds its headers: open, on this thread.
        let owner = unsafe { owner(header) };
        let Some(slot) = owner.slot.take() else {
            unreachable!("a reclaiming header names its slot")
        };
        let entry = &mut self.classes[owner.class as usize];
        entry.object.reclaim(slot);
        owner.state.set(TaskState::Free);
        entry.retired -= 1;
    }

    /// The drain's check (ADR-0003 A1.2.7): one `Relaxed` load of the remote stack's head and a
    /// branch, inline in `run_ready` and the executor's drop; the drain runs only when a foreign
    /// wake is queued.
    #[inline(always)]
    fn drain_remote(&self) {
        if self.stack_ref().pending() {
            self.drain_pending();
        }
    }

    /// The drain (ADR-0003 A1.2.7): takes all of the remote stack; each bridge's link is read
    /// before its `Q` and `W` are cleared. A cleared `W` is the owner-lane wake of that header's
    /// task; then a Retired header no waker names links its reclaim. Cold and out of line: no
    /// product path queues a foreign wake while the executor lives (ADR-0149 D1), and a check
    /// inside this body would make every `run_ready` call it and run its prologue.
    #[cold]
    #[inline(never)]
    fn drain_pending(&self) {
        let mut drained = self.stack_ref().take();
        // SAFETY: a queued bridge is allocated while its `Q` is set: the push's node, the
        // header's own address.
        while let Some((at, delivery)) = drained.deliver_next(|at| unsafe { &*at.as_ptr() }) {
            // SAFETY: the node is the header's address (`repr(C)`, bridge first), with its
            // provenance; the executor lives, so the header is open, and this is its thread.
            unsafe { settle_drained(at.cast::<WakeHeader>(), delivery) };
        }
    }

    /// Live tasks — suspended, queued or running; a Retired one is not live. The leak assert:
    /// after a quiesced workload this must be 0.
    pub fn live_tasks(&self) -> usize {
        self.classes.iter().map(|entry| (entry.object.len() - entry.retired) as usize).sum()
    }

    /// The header array's bytes: one allocation, freed whole with its stack by its last count's
    /// release (a domain of the cell's reserve, resident from boot).
    pub fn header_array_bytes(&self) -> usize {
        // SAFETY: the executor holds its array.
        unsafe { self.array.as_ref() }.layout.size()
    }

    /// Foreign-lane operations on this cell's headers (`waker_foreign_ops`, cell scope): 0
    /// while no product path moves a waker off its cell (ADR-0149 D1). A statistic: it wraps.
    pub fn waker_foreign_ops(&self) -> u64 {
        self.stack_ref().foreign_ops()
    }

    /// Whether a task is queued to run — work the loop's park decision must see: a task woken
    /// after `run_ready`, a reclaim, or a foreign wake on the remote stack, which the next
    /// `run_ready` delivers.
    pub fn has_ready(&self) -> bool {
        // SAFETY: the executor holds its array.
        let head = unsafe { self.array.as_ref() };
        head.ready_head.get() != NO_HEADER || self.stack_ref().pending()
    }

    /// Whether `id` still names a live task (slots are recycled; stale ids compare against the
    /// slot's generation and the slab's instance).
    pub fn is_live(&self, id: TaskId) -> bool {
        let Some(header) = self.header_of(id) else { return false };
        // SAFETY: the executor holds its headers: open, on this thread.
        let state = unsafe { owner(header) }.state.get();
        matches!(
            state,
            TaskState::Idle | TaskState::Queued | TaskState::Running | TaskState::RunningWoken
        )
    }

    /// The drop's close of `header` (ADR-0003 A1.3): counted on the stack, its `o` read, then
    /// `MARK_CLOSED` and the fold of `o` into `c` with `X`. Released now if no waker names it and
    /// no push holds it; otherwise its last waker's drop, on any thread, releases it.
    fn close(&self, header: Header) {
        self.stack_ref().retain();
        // SAFETY: the executor holds the header: open, on this thread.
        let owner_count = unsafe { owner(header) }.owner_count.get();
        // SAFETY: the bridge is atomics only.
        match unsafe { bridge(header) }.close(owner_count) {
            // SAFETY: `close` left it at `c = 0` with `Q` clear: this thread releases it.
            Closing::Free => unsafe { free_closed(header) },
            Closing::Retain => {}
        }
    }
}

/// A class's slab, if it is of future `F` at chunk `C`: the entry's stored `TypeId` compared, no
/// virtual call (ADR-0177 D2's recorded type).
#[inline(always)]
fn class_slab<F: Future<Output = ()> + 'static, const C: usize>(
    slab_type: TypeId,
    object: &mut Box<dyn ClassObject>,
) -> Option<&mut ClassSlab<F, C>> {
    if slab_type != TypeId::of::<ClassSlab<F, C>>() {
        return None;
    }
    let object: *mut dyn ClassObject = &mut **object;
    // SAFETY: `slab_type` is the `TypeId` of the object's concrete type: `ClassBuild::build`
    // returned the two together and nothing writes either after, so the object is a
    // `ClassSlab<F, C>`, and the pointer, from the `&mut` this borrow holds, is its address.
    Some(unsafe { &mut *object.cast::<ClassSlab<F, C>>() })
}

/// A finished task's decision, its future dropped in place, so every waker it owned is counted
/// out: true when its slot frees now (no waker names its header and it is not queued; the caller
/// takes the slot); else the header is Retired, counted in `retired`, and the slot stays held
/// until the last waker's drop links its reclaim.
///
/// # Safety
/// `header` is open, on the owner thread, its task's future just dropped in place.
#[inline(always)]
unsafe fn finished(header: Header, retired: &mut u32) -> bool {
    // SAFETY: per the contract.
    let owner = unsafe { owner(header) };
    // SAFETY: as above.
    if PLANT_REUSED_WITH_WAKERS || unsafe { quiet(header) } {
        owner.slot.set(None);
        owner.state.set(TaskState::Free);
        true
    } else {
        owner.state.set(TaskState::Retired);
        *retired += 1;
        false
    }
}

/// A suspended poll's state: requeued if it was woken during the poll, else Idle.
///
/// # Safety
/// `header` is open, on the owner thread, and on no list.
unsafe fn suspended(header: Header) {
    // SAFETY: per the contract.
    let owner = unsafe { owner(header) };
    match owner.state.get() {
        TaskState::RunningWoken => {
            owner.state.set(TaskState::Queued);
            // SAFETY: as above.
            unsafe { link_ready(header) };
        }
        TaskState::Running => owner.state.set(TaskState::Idle),
        other @ (TaskState::Free
        | TaskState::Idle
        | TaskState::Queued
        | TaskState::Finished
        | TaskState::Retired
        | TaskState::Reclaiming
        | TaskState::Dropping
        | TaskState::Closing) => unreachable!("a suspended poll's header is {other:?}"),
    }
}

/// One drained bridge of a live executor: a pending wake delivered on the owner lane, then a
/// Retired header no waker names linked for its reclaim.
///
/// # Safety
/// `header` is open, its `Q` just cleared by this drain, and the caller is the owner thread.
unsafe fn settle_drained(header: Header, delivery: Delivery) {
    match delivery {
        // SAFETY: per this function's contract.
        Delivery::Wake => unsafe { wake_task(header) },
        Delivery::NoWake => {}
    }
    // SAFETY: per this function's contract.
    let owner = unsafe { owner(header) };
    // SAFETY: as above.
    if owner.state.get() == TaskState::Retired && unsafe { quiet(header) } {
        owner.state.set(TaskState::Reclaiming);
        // SAFETY: a Retired header is on no list.
        unsafe { link_ready(header) };
    }
}

impl Drop for CellExecutor {
    /// The executor's drop (ADR-0003 A1.3): it drains; marks every header Closing, so a waker its
    /// futures drop changes nothing but counts, and drops each live future in place; closes every
    /// header a waker still names or a push still holds; drops the class slabs, which hold husks
    /// only; takes the stack a last time, leaving its head closed; then releases its own count of
    /// the array. Nothing is kept for the process: the array lives until its last count's release.
    fn drop(&mut self) {
        self.drain_remote();
        // SAFETY: the executor holds its array.
        let len = unsafe { self.array.as_ref() }.len;
        // Every header is closed to wakers before any future drops, so a waker a future's drop
        // releases, of any task, changes only its count.
        for index in 0..len {
            // SAFETY: `index < len`; open, on this thread.
            let owner = unsafe { owner(header_at(self.array, index)) };
            let live = matches!(
                owner.state.get(),
                TaskState::Idle | TaskState::Queued | TaskState::Running | TaskState::RunningWoken
            );
            owner.state.set(if live { TaskState::Dropping } else { TaskState::Closing });
        }
        for index in 0..len {
            // SAFETY: as above.
            let owner = unsafe { owner(header_at(self.array, index)) };
            if owner.state.get() == TaskState::Dropping
                && let Some(slot) = owner.slot.get()
            {
                self.classes[owner.class as usize].object.drop_future(slot);
                owner.state.set(TaskState::Closing);
            }
        }
        for index in 0..len {
            // SAFETY: `index < len`; open, on this thread.
            let header = unsafe { header_at(self.array, index) };
            // SAFETY: as above.
            if !unsafe { quiet(header) } {
                self.close(header);
            }
        }
        self.classes.clear();
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
        unsafe { release_array(self.array) };
    }
}

impl core::fmt::Debug for CellExecutor {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "CellExecutor {{ classes: {}, live: {}, retired: {}, mark: {} }}",
            self.classes.len(),
            self.live_tasks(),
            self.classes.iter().map(|entry| entry.retired).sum::<u32>(),
            self.mark.get()
        )
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test-only: a child test reads its role")]
mod tests {
    //! The owner lane's bound, which no public path reaches before 2⁶⁰ clones.

    use inf_foundation::bounded::CapFill;

    use super::*;

    /// The probe's one future type, boxed: the test's closure has no name to register.
    type Probe = Pin<Box<dyn Future<Output = ()>>>;
    const ONE: Cap = Cap::entries::<1>("test-probe", CapFill::Serving);

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
        let (census, _mark) = CapCensus::new(inf_foundation::CellId(0));
        let mut layout = ExecutorLayout::new(&census);
        let class = layout.class(&|f: Probe| f, ONE).expect("one class");
        let mut executor = layout.build().expect("no class refuses");
        // SAFETY: the one class's one header: open, on this thread.
        let header = unsafe { header_at(executor.array, 0) };
        // SAFETY: as above.
        unsafe { owner(header) }.owner_count.set(start);
        let reached = Rc::new(Cell::new(0));
        let seen = Rc::clone(&reached);
        let TaskAdmission::Granted(permit) = executor.reserve::<Probe>(class) else {
            panic!("the probe's class has its one slot");
        };
        let outcome = permit.poll_immediate(|| {
            Box::pin(core::future::poll_fn(move |cx| {
                let clone = cx.waker().clone();
                // SAFETY: the polled task's header, on this thread.
                seen.set(unsafe { owner(header) }.owner_count.get());
                drop(clone);
                Poll::Ready(())
            }))
        });
        // The start count stands in for clones no future holds: zero it, so the task's slot
        // returns and the executor's drop releases its array.
        // SAFETY: as above.
        unsafe { owner(header) }.owner_count.set(0);
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
