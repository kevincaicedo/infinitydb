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
  8-byte local). No borrowed memory outlives a call.

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
one allocation (`Box`, 72 bytes): the `inf_foundation::wake::WakeBridge`
first (`repr(C)`), atomics only, then the owner part (the task's state and
slot, the executor's shared state's `Rc`, the owner count `o` and the held
list's links), cell-local `Cell`s. The executor's remote stack is an
allocation of its own; its shared state (the ready queue, the held list's
head) is one `Rc` that the executor and every open header hold.

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
  whose provenance reaches the whole header: the drain reads the owner part
  through that link, which a pointer derived from a `&WakeBridge` could not
  do under Stacked Borrows. The executor keeps its stack private: no safe
  path outside `executor::foreign` hands a `RemoteStack` or a resolvable
  bridge to the lane's entries.
- **Decided after the future's drop.** A finished task's header is kept
  spare, freed or made Held only after its future's drop, so every waker the
  future owned is counted out first; an owner-lane drop frees only a Held
  header (`settle_held`). A header is reused or freed while its executor
  lives only by the owner thread, on `o + c = 0` with `Q` clear, which the
  drain's clear or a waker's synchronizing move orders after every foreign
  access.
- **Nothing the executor's `&mut` covers.** A waker reaches its header, the
  shared state through its header's `Rc` and the stack through the bridge;
  the executor reads both through `&` and `Cell`s and never forms `&mut` to
  a header or to the shared state, so `settle_held`, which unlinks a Held
  header inside another task's poll, writes nothing of the executor. No
  function holds a reference argument to a header across its free: the
  paths that may free one take its raw pointer.
- **Close, then the last waker frees.** The executor's drop drains, drops
  every future (deciding each header), frees its spare header open (kept
  only while no waker names it and no push holds it, so freed as any quiet
  header is, with no atomic), closes every header it still holds
  (counted on the stack, the owner part dropped in place, `MARK_CLOSED`, `o`
  folded into `c` with `X`), takes the stack a last time, leaving its head
  closed, and releases its own count of the stack and its `Rc` of the
  shared state. A closed header is freed by the read-modify-write that
  leaves it at `c = 0` with `Q` clear, on whichever thread makes it, and the
  stack by the release of the last count (`Arc`'s protocol). Nothing is kept
  for the process.
- **The owner lane runs no atomic read-modify-write or fence**
  (`scripts/check-waker-atomics.sh` on the release asm, with its probe; its
  boundary is `executor::foreign`'s five functions, its callee list
  closed). An owner-lane clone at `LANE_COUNT_MAX` aborts before the count
  is written, as `Rc` and `Arc` at their bounds.

Miri (strict provenance, its data-race detector, Stacked Borrows, the leak
check) runs `tests/executor.rs`, `tests/waker_threads.rs` and
`tests/waker_close.rs`; Loom models the lane's protocol on
`inf_foundation::wake`'s own types. Neither models the kernel, and none of
this rests on one.

`waker_ref` builds a borrowed view in `ManuallyDrop`, so the executor's own
polls take no count; the future pays an owner-lane count only if it clones
the waker.

## 3. Type-erased task storage (`executor.rs`)

Futures are moved into heap buffers (`RawFut`) **before their first poll**
and never move again; the slab stores the handle struct, not the future, so
`Pin`'s no-move contract holds structurally. Monomorphized `poll_shim::<F>`
/ `drop_shim::<F>` are the only readers of the erased pointer, created at
the single site that knows `F`. Deallocation uses the recorded *allocation*
layout (scratch buffers may exceed `F`'s layout); zero-sized futures use an
aligned dangling pointer and skip the allocator. The fast-path scratch
buffer is reused only after `drop_in_place` of the previous occupant, and
promotion to a task slot transfers the same allocation (no copy after first
poll).
