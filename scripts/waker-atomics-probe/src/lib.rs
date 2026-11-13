//! Two `RawWakerVTable`s whose wakers carry known atomics, so the gate that claims "no atomic on
//! the waker's owner lane" is proven able to say otherwise. Every `WAKER-PROBE:` line below is a
//! contract the gate asserts against the emitted asm:
//!
//!   expect-atomic <fn>         a vtable reaches it and an atomic on its owner lane, in it or in a
//!                              callee whose body is in the asm, must be reported
//!   expect-clean  <fn>         a vtable reaches it, >0 instructions, no atomic
//!   expect-unscanned <fn>      carries an atomic but is NOT on the waker path;
//!                              reporting it would mean the gate over-reports
//!   expect-boundary <fn>       a vtable reaches it; it calls a boundary function whose atomic
//!                              stays behind the boundary: the waker scans clean and the boundary
//!                              is listed with its atomic
//!   expect-unlisted <fn>       a vtable reaches it; it calls an out-of-line function of another
//!                              crate, which the closed callee list does not hold
//!   expect-dead-boundary <fn>  a boundary function no vtable reaches: the gate must say so
//!
//! The first vtable's three planted spellings are the ones a refcount would actually produce,
//! and each target spells them differently:
//!
//! | plant | x86_64 | aarch64 Apple | aarch64 Linux |
//! |---|---|---|---|
//! | CAS retry loop | `lock cmpxchg` | `casal` | `bl __aarch64_cas8_acq_rel` |
//! | fetch-add | `lock xadd` | `ldaddal` | `bl __aarch64_ldadd8_acq_rel` |
//! | SeqCst header-field store | `xchg` | `stlur` | `stlr` |
//!
//! The first defeated an earlier gate twice over: it sat in a second basic block the awk never
//! reached, and the `lock` prefix is its own tab-separated field, which the mnemonic set could
//! not match at the line start. The Linux column has no atomic instruction at all — the gate
//! reads the outline-atomics helper call as the atomic.
//!
//! The second vtable holds the boundary's plants (ADR-0003 A1.6). The probe's `executor::foreign`
//! sits at the tree's path, so one array of boundary names serves both: an atomic behind a call
//! to a boundary function, an atomic beside one, an owner arm that calls a read-modify-write in
//! another module whose body is in the asm (the drift a lane module invites), an owner arm that
//! calls an out-of-line function of a nested crate, and a boundary function no root reaches.
#![allow(dead_code)]

use core::sync::atomic::{AtomicUsize, Ordering};
use core::task::{RawWaker, RawWakerVTable};

static COUNT: AtomicUsize = AtomicUsize::new(0);

/// A CAS retry loop behind a branch. WAKER-PROBE: expect-atomic waker_clone
unsafe fn waker_clone(data: *const ()) -> RawWaker {
    if !data.is_null() {
        let mut cur = COUNT.load(Ordering::Relaxed);
        while let Err(seen) =
            COUNT.compare_exchange_weak(cur, cur + 1, Ordering::AcqRel, Ordering::Relaxed)
        {
            cur = seen;
        }
    }
    RawWaker::new(data, &WAKER_VTABLE)
}

/// A refcount bump. WAKER-PROBE: expect-atomic waker_wake
unsafe fn waker_wake(data: *const ()) {
    if !data.is_null() {
        COUNT.fetch_add(1, Ordering::AcqRel);
    }
}

/// What a waker's data pointer reaches: a task header whose state is not
/// at offset 0, so an RCpc core (the Apple M1 the macOS leg compiles for)
/// stores it with `stlur x8, [x0, #8]`, a spelling `stlr` does not match.
#[repr(C)]
pub struct Header {
    pub tag: usize,
    pub state: AtomicUsize,
}

/// A sequentially consistent store to the header's state through the
/// waker's data pointer. WAKER-PROBE: expect-atomic waker_wake_by_ref
unsafe fn waker_wake_by_ref(data: *const ()) {
    if !data.is_null() {
        unsafe { (*data.cast::<Header>()).state.store(1, Ordering::SeqCst) };
    }
}

/// Branchy and atomic-free — the shape the real wakers have, and the shape
/// the old awk could not scan past. WAKER-PROBE: expect-clean waker_drop
unsafe fn waker_drop(data: *const ()) {
    if !data.is_null() {
        unsafe { core::ptr::write_volatile(data as *mut u8 as *mut u8, 1) };
    }
}

pub static WAKER_VTABLE: RawWakerVTable =
    RawWakerVTable::new(waker_clone, waker_wake, waker_wake_by_ref, waker_drop);

/// Atomic, and deliberately unreachable from the vtable: the gate must NOT
/// report it. WAKER-PROBE: expect-unscanned off_path_atomic
#[inline(never)]
pub fn off_path_atomic(n: usize) -> usize {
    COUNT.fetch_add(n, Ordering::SeqCst)
}

/// The boundary at the tree's path: the gate's five names, each `#[inline(never)]` and each
/// carrying an atomic, as the foreign lane's entries do.
pub mod executor {
    pub mod foreign {
        use core::sync::atomic::{AtomicUsize, Ordering};

        static LANE: AtomicUsize = AtomicUsize::new(0);

        #[inline(never)]
        pub unsafe fn clone(data: *const ()) {
            if !data.is_null() {
                LANE.fetch_add(1, Ordering::AcqRel);
            }
        }

        #[inline(never)]
        pub unsafe fn wake_by_ref(data: *const ()) {
            if !data.is_null() {
                LANE.fetch_or(2, Ordering::AcqRel);
            }
        }

        #[inline(never)]
        pub unsafe fn wake(data: *const ()) {
            if !data.is_null() {
                LANE.fetch_sub(1, Ordering::AcqRel);
            }
        }

        #[inline(never)]
        pub unsafe fn drop(data: *const ()) {
            if !data.is_null() {
                LANE.fetch_sub(1, Ordering::AcqRel);
            }
        }

        /// No root reaches it: a boundary the gate exempts must exist.
        /// WAKER-PROBE: expect-dead-boundary settle_held
        #[inline(never)]
        pub unsafe fn settle_held(data: *const ()) {
            if !data.is_null() {
                LANE.fetch_and(1, Ordering::AcqRel);
            }
        }
    }
}

/// Another module's read-modify-write, out of line with its body in this asm: the owner arm's
/// drift to the foreign lane's atomics, which the gate follows.
pub mod lane {
    use core::sync::atomic::Ordering;

    #[inline(never)]
    pub fn fetch_add_one() -> usize {
        super::COUNT.fetch_add(1, Ordering::AcqRel)
    }
}

/// Calls two boundary functions, whose atomics stay behind the boundary.
/// WAKER-PROBE: expect-boundary boundary_clone
unsafe fn boundary_clone(data: *const ()) -> RawWaker {
    unsafe {
        executor::foreign::clone(data);
        executor::foreign::wake_by_ref(data);
    }
    RawWaker::new(data, &BOUNDARY_WAKER_VTABLE)
}

/// An atomic beside a boundary call, on the owner lane. WAKER-PROBE: expect-atomic beside_wake
unsafe fn beside_wake(data: *const ()) {
    unsafe { executor::foreign::wake(data) };
    if !data.is_null() {
        COUNT.fetch_add(2, Ordering::AcqRel);
    }
}

/// An owner arm calling another module's read-modify-write whose body is in the asm.
/// WAKER-PROBE: expect-atomic drift_wake_by_ref
unsafe fn drift_wake_by_ref(data: *const ()) {
    if !data.is_null() {
        lane::fetch_add_one();
    }
}

/// An owner arm calling an out-of-line function of a nested crate, which has no body in this
/// asm and is no entry of the closed callee list. WAKER-PROBE: expect-unlisted unlisted_drop
unsafe fn unlisted_drop(data: *const ()) {
    unsafe { executor::foreign::drop(data) };
    if !data.is_null() {
        waker_atomics_probe_nested::outline(data as usize);
    }
}

pub static BOUNDARY_WAKER_VTABLE: RawWakerVTable =
    RawWakerVTable::new(boundary_clone, beside_wake, drift_wake_by_ref, unlisted_drop);
