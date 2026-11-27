# inf-runtime SAFETY

`inf-runtime` is one of the four audited unsafe-leaf crates. Every unsafe
block carries a `// SAFETY:` comment (clippy
`undocumented_unsafe_blocks = deny`); this file records the audited areas
and the invariants they rest on. Inventory-vs-code agreement is
script-checked (`scripts/check-safety-inventory.sh`): every
src file using unsafe must be named here.

## 1. Backend FFI (`kqueue.rs`, `uring.rs`, `net.rs`)

Plain syscall surface: `kqueue/kevent`, `accept/read/write/close/fcntl`,
`uname`, and the `io-uring` crate's unsafe SQ push / buffer registration.

Invariants:

- **Stable buffer addresses.** All kernel-visible pointers come from
  `inf_alloc::BufferPool`, whose buffers are individually boxed and never
  reallocated for the pool's lifetime (documented invariant of that crate;
  io_uring fixed/provided registration relies on it).
- **Lease-then-expose.** A buffer is leased *before* its address is handed
  to the kernel and stays leased until a terminal completion resolves it —
  to the consumer (`Recv`/`Sent`) or back to the pool (error, cancel,
  provide-failure). No `&mut` to a buffer is materialized while the kernel
  may write it: Rust-side access happens only after the CQE/event that ends
  kernel ownership.
- **Close-after-cancel (uring).** In-flight ops hold kernel file refs;
  `Close` first queues `AsyncCancel` for every op on the fd, and the
  `Closed` completion is withheld until in-flight sends resolve, so buffer
  ownership always unwinds before the consumer forgets the fd.
- `kevent` changelists/eventlists point into live `Vec` storage with exact
  lengths; timeout pointers live on the calling frame.
- `net.rs` (M2.5-S16 inventory addition — the sites predate it): plain
  socket FFI — `socket`/`setsockopt`/`bind`/`listen` on a locally created
  fd converted once via `from_raw_fd` (single ownership transfer),
  `eventfd` creation with the same single-`OwnedFd` pattern, and the
  doorbell `write` of one stack `u64` (pointer + length name the same
  8-byte local). No borrowed memory outlives a call. `close_reporting`
  takes an `OwnedFd`, ends its ownership with `into_raw_fd` and closes the
  number once with `close(2)`; the number is consumed on every return, so
  no second close of it exists (ADR-0156 D3's final close).
  `close_number_planted` is that second close, compiled only into the
  `inf_canary_retire_close_twice` build: a planted violation for the
  retirement worker's oracle, never in any other build.

## 1b. Stable byte ranges (`driver.rs::StableBytes`)

`StableBytes::new` is the **one unsafe constructor the durable plane rests
on** (ADR-0013 D1): it erases the lifetime of a byte slice handed to a
driver op (`LogWrite`, checkpoint section writes). The caller contract —
bytes stay live, stable, and unmodified until the op's *terminal*
completion — is discharged by lease custody, never by inspection:

- the staging `FrameLease` (frame buffers never reallocate; reset only on
  release at the write's terminal CQE) — constructed in
  `inf-server::log_bytes` (that crate's SAFETY.md);
- the checkpoint `SectionLease` (same pattern, `.ick` section pair);
- the sim driver reads them strictly before terminal completion
  (`bins/inf-sim/SAFETY.md`).

The constructor itself only records `(ptr, len)`; every dereference lives
with the custody proof at the consuming site.

## 1b-tier. Cold-read custody (`cold.rs`, M4-S08)

`ColdReads::issue` constructs the one `StableBytesMut` on the cold-read
path, over an `inf_alloc::AlignedPool` buffer. The stability contract is
discharged **structurally, not by the issuing future**: the lease is held
by the cell-local in-flight table from issue until the terminal
completion, where custody transfers into the delivered `ColdDone` guard
(released exactly once by its `Drop` — resumed, cancelled, or unclaimed
alike; the three interleavings are unit-tested). No `&mut` to the buffer
is materialized between issue and completion — `ColdDone::bytes` is the
first reader and runs strictly after kernel ownership ends. The pool's
addresses are stable for its lifetime (that crate's invariant), so
fixed-buffer registration (`uring.rs::register_tier_pool`, iovecs over
`AlignedPool::buffers_mut`) rests on the same proof. The test-module
helper that plays the driver writes through the same handle under the
same in-flight custody.

## 1c. Thread affinity (`affinity.rs`)

`sched_setaffinity` FFI over a zeroed, fully-populated stack `cpu_set_t`
(pin at cell boot; `unpin_current_thread` added by M2.5-S08 for the
boot-scoped read-ahead worker). The kernel copies the mask; no caller
memory is retained. tid 0 = the calling thread; a full mask is intersected
with the online set by the kernel, so no error path depends on topology.

## 1d. Termination signals (`signal.rs`, ADR-0124 D1)

One `sigaction` per signal (`SIGTERM`, `SIGINT`) from a zeroed struct with
the handler stored through `sa_sigaction`, `SA_RESETHAND | SA_RESTART`,
an emptied mask and no old-action pointer. The handler is `extern "C"`
and performs exactly one `AtomicBool` store into a `static` — the only
operation it does is async-signal-safe; nothing allocates, locks or
formats. The flag outlives every reader (`'static`). The test raises the
signal at the test process once; the reset disposition then makes a
second delivery default, which no test performs.

## 2. The waker vtable at a wake bridge (`executor.rs`, ADR-0003 A1)

A standard `Waker` is `Send + Sync`, so the vtable's four functions must be
sound on any thread. A waker's data pointer is its task's **wake header**,
one entry of the executor's **header array** (72 bytes, `repr(C)`): the
`inf_foundation::wake::WakeBridge` first, atomics only; then the header's
place (its index and class, written once at the array's build); then the
owner part (the task's state, its slot's handle, the owner count `o` and
the ready list's link), cell-local `Cell`s. The array is one allocation:
the remote stack first, the ready list's two ends and the headers' count,
then the headers, parallel to the class slabs: class `j`'s slot `i` is
header `base_j + i`, `i` below the class's slots, so `base_j + i < len`.
`wake(id)` and `is_live` compare an id's index with its class's slots
before they form the header's pointer; the instance compare tells another
cell's slab apart and bounds nothing, since two censuses of one cell mint
equal instances (ADR-0194 D5). Every header pointer is derived from the
array's, so a waker's data pointer and a push's node carry the provenance
of the whole allocation, and `array_of` steps back from a header to the
array by its index on any thread. `WakeBridge` and `RemoteStack` are
const-asserted `Sync`.

Invariants:

- **Only the owner thread reaches the owner part.** Each vtable function
  first compares the bridge's mark with the calling thread's
  (`owner_part`); a thread forms a reference to the bridge alone unless the
  marks match. A mark is claimed once per executor-building thread and never
  reused, 0 names a thread with none, and a closed header's mark is
  `MARK_CLOSED`, which no thread holds. `CellExecutor` is `!Send` by its
  `PhantomData<*const ()>` field (a `compile_fail` doctest), so the thread
  that built it is the one that polls it.
- **The foreign lane is `inf_foundation::wake`'s.** `executor::foreign`'s
  five `#[inline(never)]` functions pass the bridge only its own remote stack
  (`WakeBridge::stack`) and, as its push's node, the header's own address,
  whose provenance reaches the whole array: the drain reads the owner part
  through that link, and a closed header's last release reaches the array
  from it, which a pointer derived from a `&WakeBridge` could not do under
  Stacked Borrows. The executor keeps its stack private: no safe path
  outside `executor::foreign` hands a `RemoteStack` or a resolvable bridge
  to the lane's entries.
- **Decided after the future's drop.** A finished task's header is marked
  Finished before its future is dropped in place and decided only after
  that drop, so every waker the future owned is counted out first: its slot
  is freed at once, or the task is Retired and its slot stays held. An
  owner-lane drop settles only a Retired header (`settle_held`), and a
  settle frees nothing: it links the header's reclaim entry on the ready
  list, which the next `run_ready` takes. A slot is freed while its
  executor lives only by the owner thread, on `o + c = 0` with `Q` clear,
  which the drain's clear or a waker's synchronizing move orders after
  every foreign access; a header is never freed while its executor lives.
- **Nothing the executor's `&mut` covers.** A waker reaches its header, the
  array (the ready list's ends) and the stack through the header's address;
  the executor reaches them through its `NonNull` of the array, by `&` and
  `Cell`s, and never forms `&mut` to a header or to the array's head, so
  `settle_held`, which links a reclaim entry inside another task's poll,
  writes nothing the executor's `&mut` covers. A function that frees the
  array takes its raw pointer. A foreign `wake` or `drop` keeps references
  past the access that may let another thread free the array: it holds
  `&WakeBridge` and `&RemoteStack` (and `WakeBridge::release_waker`'s
  `&self`) across its swap, or its push's compare-and-swap, after which the
  drain or another thread's last drop may free the array and the stack in
  it. Those references cover atomics only, as `Arc::drop`'s `&AtomicUsize`
  does across its decrement, and after that access the call neither reads
  through them nor copies one, not even into an argument of a function
  that reads nothing, unless it answered `Released::Last`, which makes it
  the one releaser (ADR-0003 A1.2.5): a reference to freed memory is
  undefined behavior once it is formed. The rule is review-only, held by an
  oracle: safe Rust has no type that ends a shared reference's use at an
  atomic access while its function still holds it, and passing the
  header's raw address instead would put `unsafe` in `inf-foundation`,
  which forbids it. Miri's default schedule never preempts a call inside
  that window, so `tests/waker_race.rs` races two foreign last drops of a
  closed header, and a foreign `wake` or `drop` against the drain and the
  executor's drop, at a pinned preemption rate over a pinned seed range
  (the CI's Miri job); a reference formed after the access is red there.
- **Close, then the last count frees.** The executor's drop drains; closes
  every header to wakers (Dropping or Closing) before any future drops,
  then drops each live future in place, and resumes past one whose drop
  unwinds: that header is marked Closing, its cell keeping the dropped
  bytes under `ManuallyDrop`, the later futures are dropped and the steps
  below run, and then the unwind continues; a second unwind aborts the
  process, the language's rule for a panic in a destructor during an
  unwind. It closes every header a waker still names or a push still holds
  (counted on the stack, its `o` read, `MARK_CLOSED`, `o` folded into `c`
  with `X`); drops its class slabs, which then hold no live future: a first
  poll that unwound gave its slot to its header first (the first poll in
  the class's spare, below) and is among the futures dropped; takes the
  stack a last time, leaving its head closed; and releases its own count.
  The array, with the stack in it, is freed by the release of its last
  count (`Arc`'s protocol): the executor's own, or a closed header's,
  released by the read-modify-write that leaves it at `c = 0` with `Q`
  clear, on whichever thread makes it. The freer drops the head and every
  header in place and deallocates the layout the head's count gives,
  written at the build and never again. Nothing is kept for the process.
- **The owner lane runs no atomic read-modify-write or fence**
  (`scripts/check-waker-atomics.sh` on the release asm, with its probe; its
  boundary is `executor::foreign`'s five functions, its callee list
  closed). An owner-lane clone at `LANE_COUNT_MAX` aborts before the count
  is written, as `Rc` and `Arc` at their bounds.

Miri (strict provenance, its data-race detector, Stacked Borrows, the leak
check) runs `tests/executor.rs`, `tests/waker_threads.rs` and
`tests/waker_close.rs` on its default schedule, and again under Tree
Borrows (`-Zmiri-tree-borrows`); the unit test
`executor::tests::the_shared_door_forgets_the_lend` under both models, in
runs of its own; and `tests/waker_race.rs` on the pinned schedules above;
Loom models the lane's protocol on `inf_foundation::wake`'s own types.
Neither models the kernel, and none of this rests on one.

`waker_ref` builds a borrowed view in `ManuallyDrop`, so the executor's own
polls take no count; the future pays an owner-lane count only if it clones
the waker.

## 3. Futures in their class's slab (`executor.rs`, ADR-0177 D2)

Each task class is a `CappedSlab` of one concrete future type,
`TaskCell<F>` (`UnsafeCell<ManuallyDrop<F>>`), built before the cell serves. A future
is written into its slot **before its first poll** and never moves: a
slab's chunks are fixed allocations, never moved or reallocated while the
slab lives, so `Pin::new_unchecked` over the slot's future is sound until
the future is dropped. It is dropped in place exactly once: by
`poll_in_place` on `Ready`, after its header is marked Finished, or by the
executor's drop for a live task (`drop_future`), after its header is
marked Dropping; each runs only while the header's state says the future
is live, and the state moves on before the slot is reached again. A poll
whose call unwinds leaves its future live where it lies, with its header
naming the slot: a queued poll's header names it already, and a first
poll's is given the spare's handle before the unwind leaves the poll, so
either is a live task, which the executor's drop drops in place. The slot
then holds a husk, `Done`, which has no drop glue: the class's spare keeps
it, or the slab's `release` frees the slot and drops the husk where it lies
(ADR-0194 D1's second note); the executor calls `take` on no task's slot.

**The future in an `UnsafeCell`.** A suspended async future may hold
`&mut` into itself across an `await` (a local lent to a callee), and the
slab reaches a slot through a shared reborrow of its whole chunk (a slice's
length, a shared walk). Over bytes outside an `UnsafeCell` that reborrow is
a read of every slot in the chunk, which Stacked Borrows lets invalidate
such a `&mut` in a neighbouring slot, so the task's next poll would use a
dead reference. Inside the cell a shared reborrow freezes nothing, so a
lookup of one slot leaves every future in its chunk as it was.
`tests/executor.rs`'s `self_referential_futures_resume_in_their_slots` and
`self_referential_futures_woken_by_their_wakers_resume_in_their_slots` place
unboxed, `!Unpin` futures suspended on such a reference and reach their
slots and their neighbours' between polls; Miri reads both under Stacked
Borrows and under `-Zmiri-tree-borrows`.

**The first poll in the class's spare.** A class's spare is one Live slot
holding the husk, its header Free and on no list, its generation in no
`TaskId`; a class has at most one. A permit is the spare: its future is
written over the husk (no drop glue: the write drops nothing) and polled
there; Ready and quiet, the slot stays the spare; otherwise the spare's
handle leaves it by `take()`: for the task's id or the Retired header, before
either exists, or, when the poll unwinds with the future live, for the
task's header, before the unwind leaves the poll. From the first poll to
the in-place drop the future is at one address: the spare's slot, which
every later poll reaches by `get_mut` of the handle (a Live slot lies where
it is until a take, a release or a retire step, ADR-0194 D1's notes).
`tests/executor.rs`'s
`the_first_poll_runs_in_its_slot_and_keeps_its_wake` reads that address at
each poll and at the drop, and `the_spare_serves_no_task_an_id_names` that
a suspended task's slot is never the spare.

**The kept lend** (`executor::class`). The spare is reached through the
occupant's address the slab last lent (`get_mut` of the spare, or
`publish_mut` of the husk), kept in a `Cell` beside it, so consecutive first
polls walk no directory. A `&mut` is made from the kept pointer only while
no borrow of the class slab, shared or mutable, has been made since the
slab lent it: the slab, the spare and the pointer are private to the
module, every other borrow passes `slab(&self)` or `slab_mut(&mut self)`,
and each forgets the pointer first. A shared borrow forgets it too: a
reborrow of a whole chunk through `&` leaves an earlier write-capable
pointer usable for reads alone (Tree Borrows freezes it). The `&mut` lives
for the permit, which borrows the executor `&mut`, so nothing else reaches
the slab meanwhile. A wake and a last waker's drop during a poll touch only
the header array, never a class slab, and so do `wake(id)` and `is_live`,
which compare the class slab's instance and the id's index under the
header bound above, then read the task's header. Miri reads the sequence
under Stacked Borrows and under `-Zmiri-tree-borrows`:
`tests/executor.rs`'s `the_spare_lend_survives_every_other_borrow` (a spawn
and its run, a reclaim, an id's lookups, a Pending first poll and its
completion, each between fast first polls) and
`the_kept_lend_across_an_id_lookup_and_a_shared_borrow`.

**The class's concrete type.** The executor reaches a class's slab by a
cast of its erased object to `ClassSlab<F, C>` after comparing the `TypeId`
the class's entry stores with that type's: `ClassBuild::build` returns the
object and the `TypeId` of its concrete type together, in one call, and
nothing writes either after the build, so the compare names the object's
type with no virtual call. A zero-sized or over-aligned future takes the
slab's own slot layout (`tests/executor.rs` runs both, at 4 KiB alignment).
