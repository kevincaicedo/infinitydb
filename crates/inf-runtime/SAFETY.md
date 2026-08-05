# inf-runtime SAFETY

`inf-runtime` is one of the four crates allowed `unsafe` (milestone M0
§3.3). Every unsafe block carries a `// SAFETY:` comment (clippy
`undocumented_unsafe_blocks = deny`); this file records the audited areas
and the invariants they rest on. Inventory-vs-code agreement is
script-checked (`scripts/check-safety-inventory.sh`, M2.5-S16): every
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

## 2. Rc waker vtable (`executor.rs`)

`RawWakerVTable` whose data pointer is `Rc<TaskHeader>` — refcounts are
**non-atomic by design** (L1, ADR-0003; verified by
`scripts/check-waker-atomics.sh` against release asm).

This deliberately does not satisfy `Waker`'s documented thread-safety
contract. Soundness rests on the **thread-locality invariant**: a waker
clone must never leave the cell thread that created it. Enforcement:

- Cell code cannot name `std::sync`/`tokio`/`async-std` (deny-list script +
  clippy config, M0-S06) — there is no sanctioned way to move a waker to
  another thread.
- Futures executed by `CellExecutor` are `!Send` by construction and the
  executor itself is never shared.
- DST (M0-S20) replays interleavings single-threaded, where the invariant
  is trivially true.

Vtable accounting: `clone` increments, `wake`/`drop` decrement, and
`wake_by_ref` borrows; `waker_ref` constructs a borrowed view in
`ManuallyDrop` so the executor's own polls never touch the refcount.

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
