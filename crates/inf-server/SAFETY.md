# inf-server SAFETY inventory

`inf-server` is `#![deny(unsafe_code)]`; exactly one module opts out
(M2-S08, ADR-0015 D4). Any new `unsafe` here needs an ADR first.

## `src/log_bytes.rs` — `sealed_frame`

One `unsafe` block: `StableBytes::new` over the sealed staging frame for
`IoOp::LogWrite` (ADR-0013 D1). The contract is *the bytes stay live,
stable, and unmodified until the op's terminal completion*; the proof is
the staging `FrameLease` custody chain:

1. `StagingRing::new` allocates both frame buffers once with capacity
   `capacity_bytes`; admission (`stage`) never lets `frame_len` exceed it,
   so the backing `Vec` can never reallocate — the pointer is stable for
   the cell's lifetime.
2. `seal` hands the buffer out as a `FrameLease`; the buffer is not reset
   (and cannot be re-sealed — at most one lease exists) until
   `release(lease)`.
3. The plane stores the lease in `DurableCell::in_flight` when it queues
   the `LogWrite` and releases it **only** in the REAP arm for that op's
   terminal completion (`LogWritten` or `Error`) — after the kernel is
   done with the bytes.

Copying the frame into a pool buffer instead would cost a full-frame copy
per iteration (rejected in ADR-0013: the zero-copy seal is the point of
the staging design). Verified by: the S05/S06 integration harness
(`inf-log/tests/`) exercising the identical custody chain, the M2-S08
durable e2e tests, and byte-identical replay assertions.

## `src/log_bytes.rs` — `ckpt_block` (M2-S10, ADR-0016 D4)

Second `unsafe` block, same contract class: `StableBytes::new` over a
sealed checkpoint block (`IckStream` header/section/footer) for a driver
`LogWrite` on the `.ick` fd. The proof is the `SectionLease` custody
chain:

1. `IckStream` is a double-buffered section pair. Sealing (`begin` /
   `seal_section` / `finish`) swaps the sealed buffer out of the staging
   rotation; while leased it is never appended to, cleared, or
   reallocated — only the *other* buffer accepts records (and may grow;
   growth never touches the leased buffer's heap storage).
2. At most one lease exists (`IckStream` asserts it); `release(lease)`
   only `clear()`s — capacity and pointer retained.
3. The plane stores the lease in `ckpt::Streaming::in_flight` when it
   queues the write and releases it **only** in the REAP arm for that
   op's terminal completion (`LogWritten` or the ckpt-abort `Error`
   path) — after the kernel is done with the bytes.
4. The `Streaming` state is boxed; moving the box (phase transitions)
   moves the pointer, never the `Vec`s' heap storage.

Verified by: the inf-log `ckpt` round-trip/corruption suites, the
S10 store-integration tests (dirty-under-checkpoint digest equality), and
the durable e2e checkpoint test on real io_uring.
