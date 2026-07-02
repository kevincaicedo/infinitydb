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
