//! The **only** unsafe constructions in `inf-server` (M2-S08 ADR-0015 D4,
//! extended by M2-S10 ADR-0016 D4, M4.5-S31 ADR-0084 D3, and M4.5-S34
//! ADR-0086 D4; contract shape from ADR-0013 D1): erasing a sealed,
//! lease-guarded buffer's borrow lifetime so the kernel can read it after
//! the queueing borrow ends. Everything else in this crate is
//! `#![deny(unsafe_code)]`-clean; this module is the whole audit surface
//! (see `SAFETY.md`).
#![allow(unsafe_code)]

use inf_alloc::AlignedBox;
use inf_log::{FrameLease, IckStream, SectionLease, StagingRing, TierOpView};
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

/// Captures a sealed checkpoint block behind `lease` for a driver
/// `LogWrite` on the `.ick` fd (M2-S10, ADR-0016 D4).
///
/// The `SectionLease` is the stability proof: `IckStream` seals a block by
/// swapping it out of the staging rotation — the leased buffer is never
/// appended to, never cleared, and never reallocated until
/// `release(lease)` (only the *other* buffer grows during the flight; a
/// `release` merely `clear()`s, retaining capacity). The plane stores the
/// lease in `Streaming::in_flight` and releases it only in the REAP arm
/// for the op's terminal completion (`LogWritten`/`Error`), and moving the
/// boxed `Streaming` state never moves the `Vec`'s heap storage.
pub(crate) fn ckpt_block(stream: &IckStream, lease: &SectionLease) -> StableBytes {
    let bytes = stream.leased_bytes(lease);
    // SAFETY: per the lease argument above — the pointee is stable and
    // unmodified until the op's terminal completion releases the lease
    // held in `Streaming::in_flight`.
    unsafe { StableBytes::new(bytes) }
}

/// Captures one tier-flush round op's window for a driver `LogWrite`
/// (M4.5-S31, ADR-0084 D3).
///
/// The round is the stability proof: windows are pool-owned `Box<[u8]>`
/// blocks inside the namespace's `TierFlush` — heap storage that never
/// moves when the owning structs move — staged once per round and never
/// written again (error retries resubmit byte-identical). They return
/// to the pool only in `finish_round`, which the plane calls strictly
/// after every op of the round reached a terminal completion (the
/// `FlushRound::pending` count in `tier_cell`), and a namespace dropped
/// mid-round parks whole in `round_drain` under the same rule.
pub(crate) fn tier_round_bytes(view: &TierOpView<'_>) -> StableBytes {
    // SAFETY: per the round argument above — the pointee is pool-owned,
    // heap-stable, and unmodified until `finish_round`, which is gated
    // on the terminal completion of every op in the round.
    unsafe { StableBytes::new(view.bytes) }
}

/// Captures `len` bytes of the cell's zero window for a zero-fill
/// `LogWrite` (M4.5-S34, ADR-0086 D4).
///
/// The window is the stability proof: one `AlignedBox` owned by the
/// `DurableCell` for the cell's whole life, zeroed at birth and never
/// written — there is no writer to race, and the allocation never moves.
/// At most one zero slice is in flight per cell (`SegmentRotor` hands out
/// the next only after the previous `LogWritten`), and the box outlives
/// every op because the cell outlives its durable plane.
pub(crate) fn zero_window(window: &AlignedBox, len: u32) -> StableBytes {
    let bytes = &window.bytes()[..len as usize];
    // SAFETY: per the window argument above — the pointee is immutable,
    // heap-stable for the cell's lifetime, and outlives the op.
    unsafe { StableBytes::new(bytes) }
}
