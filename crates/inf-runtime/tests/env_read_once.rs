#![allow(
    clippy::disallowed_methods,
    reason = "test target: the reproducer writes the process environment, not cell code"
)]
//! A ring's selections are fixed when it is built (ADR-0209 D1): a variable
//! set afterwards, as a library's `setenv` might mid-run, changes no submit.
//! The driver once read `INF_URING_SUBMIT_PER_OP` on every loop iteration,
//! so setting it flipped a live ring to one `io_uring_enter` per SQE. The
//! test builds a batched ring, sets the variable, queues 16 closes and
//! reads the call's `SubmitStats`: one enter for every SQE. Its own test
//! binary: it writes the environment, sound only while no other thread
//! reads or writes it.

#![cfg(all(target_os = "linux", feature = "uring"))]

use std::os::fd::IntoRawFd;

use inf_alloc::BufferPool;
use inf_runtime::{
    BackendDriver, CompletionToken, IoOp, SubmitPolicy, TokenClass, UringDriver, Wait,
};

#[path = "support/uring_env.rs"]
mod uring_env;

/// Closes queued for the measured call: each is one SQE with nothing in
/// flight on its fd.
const CLOSES: u32 = 16;

#[test]
fn a_variable_set_after_the_ring_is_built_changes_no_submit() {
    let config = uring_env::config().with_submit(SubmitPolicy::Batched);
    let mut driver = UringDriver::new(256, config).expect("io_uring");
    uring_env::assert_engaged(&driver.capabilities());
    let mut pool = BufferPool::new(16, 4096);
    let mut out = Vec::new();
    // The warm-up call takes the first restock of provided buffers, so the
    // measured call's SQEs are the closes and any restock left over.
    driver.submit_and_reap(&mut pool, Wait::Poll, &mut out).expect("warm-up");
    // SAFETY: the binary's one test, and no other thread of it reads or
    // writes the environment.
    unsafe { std::env::set_var("INF_URING_SUBMIT_PER_OP", "1") };
    for slot in 0..CLOSES {
        let fd = std::net::UdpSocket::bind("127.0.0.1:0").expect("socket").into_raw_fd();
        driver.push(IoOp::Close { fd, token: CompletionToken::new(TokenClass::Close, slot, 0) });
    }
    out.clear();
    driver.submit_and_reap(&mut pool, Wait::Poll, &mut out).expect("the measured call");
    let stats = driver.submit_stats();
    assert!(stats.sqes >= u64::from(CLOSES), "every close rode the call: {stats:?}");
    assert_eq!(
        stats.syscalls, 1,
        "one enter carries the call's {} SQEs; a ring that reads the variable after it is \
         built submits once per SQE: {stats:?}",
        stats.sqes
    );
}
