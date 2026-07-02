//! The **one** unsafe construction in `inf-server` (M2-S08, ADR-0015 D4;
//! contract shape from ADR-0013 D1): erasing the sealed staging frame's
//! borrow lifetime so the kernel can read it after the queueing borrow
//! ends. Everything else in this crate is `#![deny(unsafe_code)]`-clean;
//! this module is the whole audit surface (see `SAFETY.md`).
#![allow(unsafe_code)]

use inf_log::{FrameLease, StagingRing};
use inf_runtime::StableBytes;

/// Captures the sealed frame behind `lease` for a driver `LogWrite`.
///
/// The `FrameLease` is the stability proof (ADR-0013 D1): the staging
/// buffers are allocated once at `StagingRing::new` and admission never
/// lets a frame exceed their capacity, so the `Vec` never reallocates; the
/// sealed buffer is not reset until `release(lease)`, which the plane
/// calls only on the write's terminal completion (`LogWritten`/`Error`).
/// Callers must keep the lease alive (in `DurableCell::in_flight`) until
/// that completion — the type system enforces the lease's existence, the
/// plane's REAP arm enforces the timing.
pub(crate) fn sealed_frame(staging: &StagingRing, lease: &FrameLease) -> StableBytes {
    let bytes = staging.leased_frame(lease);
    // SAFETY: per the lease argument above — the pointee is stable (no
    // reallocation possible) and unmodified until the op's terminal
    // completion releases the lease held by the caller.
    unsafe { StableBytes::new(bytes) }
}
