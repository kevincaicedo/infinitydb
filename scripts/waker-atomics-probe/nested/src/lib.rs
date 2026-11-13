//! An out-of-line function of another crate: a call to it from a waker's owner lane has no body
//! in the caller's asm to scan, so the waker gate refuses it as an unlisted callee.

/// Out of line, so the call stays a call.
#[inline(never)]
pub fn outline(value: usize) -> usize {
    core::hint::black_box(value).wrapping_add(1)
}
