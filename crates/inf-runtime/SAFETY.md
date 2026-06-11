# inf-runtime SAFETY

`inf-runtime` is one of the four crates allowed `unsafe` (milestone M0
§3.3). Every unsafe block carries a `// SAFETY:` comment (clippy
`undocumented_unsafe_blocks = deny`); this file records the three audited
areas and the invariants they rest on.

## 1. Backend FFI (`kqueue.rs`, `uring.rs`)

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
