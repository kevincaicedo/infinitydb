//! A `RawWakerVTable` whose wakers carry known atomics, so the gate that
//! claims "zero atomic instructions in the waker path" is proven able to
//! say otherwise. Every `WAKER-PROBE:` line below is a contract the gate
//! asserts against the emitted asm:
//!
//!   expect-atomic <fn>    the vtable reaches it and it must be reported
//!   expect-clean  <fn>    the vtable reaches it, >0 instructions, no atomic
//!   expect-unscanned <fn> carries an atomic but is NOT on the waker path;
//!                         reporting it would mean the gate over-reports
//!
//! The three planted spellings are the ones a refcount would actually
//! produce: a CAS retry loop (x86 `lock cmpxchg` / aarch64 `ldaxr`+`stlxr`
//! or `casal`), a fetch-add (`lock xadd` / `ldaddal`) and a SeqCst store
//! (`xchg` / `stlr`). The first defeated the pre-ADR gate twice over: it
//! sat in a second basic block the awk never reached, and the `lock`
//! prefix is its own tab-separated field, which the mnemonic set could not
//! match at the line start.
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

/// A sequentially consistent store. WAKER-PROBE: expect-atomic waker_wake_by_ref
unsafe fn waker_wake_by_ref(data: *const ()) {
    if !data.is_null() {
        COUNT.store(data as usize, Ordering::SeqCst);
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
