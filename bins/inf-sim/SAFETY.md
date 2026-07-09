# inf-sim — unsafe inventory

`inf-sim` is not an unsafe leaf crate (§17.3); it carries exactly one
audited `unsafe` construction, following the `inf-server::log_bytes`
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
