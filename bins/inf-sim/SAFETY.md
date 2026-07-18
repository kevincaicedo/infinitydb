# inf-sim — unsafe inventory

`inf-sim` is not an unsafe leaf crate (§17.3); it carries exactly two
audited `unsafe` constructions, following the `inf-server::log_bytes`
precedent (ADR-0015 D4).

## 1. `net.rs::stable_slice` — reading a `StableBytes` op payload

- **What:** `std::slice::from_raw_parts(data.as_ptr(), data.len())` over
  the `StableBytes` carried by an `IoOp::LogWrite`.
- **Why it is sound:** `StableBytes::new` is `unsafe` with the contract
  that the bytes stay live, at that address, and unmodified until the
  op's **terminal** completion (`LogWritten` or `Error`) — upheld by the
  plane holding the staging `FrameLease` until exactly that completion
  (ADR-0013). `stable_slice` is called only inside
  `SimDriver::submit_and_reap`, strictly *before* the terminal
  completion is pushed, so the contract window is open. The slice does
  not escape the call (it is copied into the sim disk's buffers).
- **Who else does this:** every backend driver executing the op — the
  uring tier passes the pointer to the kernel, the kqueue tier and the
  `inf-log` test `ScriptedDriver` build the same slice. `StableBytes::
  as_ptr` is public precisely for out-of-crate drivers (its rustdoc).
- **Tests:** `tests/disk.rs` drives `LogWrite`/`Fdatasync` through the
  sim driver end-to-end (buffered-until-sync semantics, failed-write
  cancels linked sync); the recovery smoke and future S19 sweeps run the
  same path continuously.

## 2. `net.rs::stable_mut_slice` — filling a `StableBytesMut` read target

- **What:** `std::slice::from_raw_parts_mut(buf.as_mut_ptr(), buf.len())`
  over the `StableBytesMut` carried by an `IoOp::TierRead` (M4-S04).
- **Why it is sound:** `StableBytesMut::new` is `unsafe` with the
  contract that the bytes stay live, at that address, and **unaliased**
  until the op's terminal completion (`TierRead` or `Error`) — upheld by
  the issuing command holding the aligned-pool lease across its
  suspension and not touching the buffer until it resumes on that
  completion. `stable_mut_slice` is called only inside
  `SimDriver::submit_and_reap`, strictly *before* the terminal
  completion is pushed, so the contract window is open. The slice does
  not escape the call (the sim disk copies into it).
- **Who else does this:** the uring tier passes the same pointer to the
  kernel; the kqueue tier builds the same slice for its synchronous
  `pread` loop.
- **Tests:** the `m4-steel` scenario drives `TierRead` through the sim
  driver end-to-end (cold read after demotion, deterministic replay);
  the Linux twin is `inf-runtime/tests/steel_thread.rs`.

## 3. `steel.rs::plan` — capturing a `StableBytesMut` read target

- **What:** `StableBytesMut::new(dest)` over an aligned-pool buffer slice
  for an `IoOp::TierRead` the scenario issues (M4-S04).
- **Why it is sound:** the constructor's contract (live, stable,
  unaliased until terminal completion) is upheld the same way the Linux
  twin upholds it: the pool's buffer addresses are stable for the pool's
  lifetime, the issuing future holds the lease as plain `Copy` data
  across its suspension, and the buffer is first touched again only
  after the `TierRead` completion resumes the future.
- **Who else does this:** `inf-runtime/tests/steel_thread.rs` (the Linux
  twin) and the S08 production path once cold reads wire into commands.
- **Tests:** the `m4-steel` scenario itself — every run reconciles the
  aligned pool to zero leases and `--verify-determinism` replays it.
