//! A connection's task waits on the durability watermark once, for the
//! smallest sequence its pending replies need (ADR-0177 D5): the pending
//! entry carries its sequence, so a pipeline of `always` writes held on
//! their fsync is one wait, not one per write. The test reads the cell's
//! watermark waits through the plane between scheduler steps; the device's
//! held frame barrier, the durable plane's appended records and the
//! client's bytes are its engagement.

use std::path::Path;

use super::park::{Quiet, STEPS_MAX};
use super::{DurableScenario, MiniClient, encode};

/// The writes one connection pipelines: one pump pass stages what one
/// receive buffer carried (1,024 B on the simulated node), so the burst is
/// the most writes of one width that fit one buffer. 31 `SET`s of 33 B take
/// 1,023 B; a 32nd would take a second receive, and its pass.
const WRITES: usize = 31;

/// One write's bytes on the wire: `SET ack:NNN 1` as a RESP array.
const SET_BYTES: usize = 33;

/// Steps the cell runs once the burst is appended, so every wait its pump
/// takes for the burst is parked before the count is read.
const SETTLE_STEPS: u32 = 16;

/// Holds every barrier on every file.
fn any_file(_: &Path) -> bool {
    true
}

/// One connection sends 31 `always` writes at one step, delivered whole,
/// while the device holds every barrier: the pump stages all 31 in one pass,
/// and the cell then parks exactly one watermark wait for them, its front's.
/// Once the hold lifts, all 31 are acknowledged in pipeline order and no
/// wait is left parked: no entry is stranded behind the front. Engagement,
/// each else its `VACUOUS` line: the device held a frame barrier
/// (`frame-barrier-unheld`), all 31 were appended (`writes-unstaged`), and no
/// ack was read under the hold (`acked-under-hold`).
#[test]
fn a_task_waits_on_the_watermark_once_for_its_staged_writes() {
    let mut quiet = Quiet::boot(1, DurableScenario::SPIN_ITERS, 0);
    let mut setup = MiniClient::connect(&mut quiet.node, 0);
    let create: &[&[u8]] =
        &[b"INF.NS", b"CREATE", b"acks", b"MODE", b"durable", b"FSYNC", b"always"];
    assert_eq!(quiet.call(&mut setup, create, "the always CREATE"), b"+OK\r\n");
    let mut writer = MiniClient::connect(&mut quiet.node, 0);
    assert_eq!(quiet.call(&mut writer, &[b"INF.NS", b"USE", b"acks"], "USE"), b"+OK\r\n");
    let appended = |quiet: &Quiet| {
        quiet.node.plane(0).durable_stats().map_or(0, |stats| stats.records_appended)
    };
    let before = appended(&quiet);
    quiet.disk.hold_barriers(Some(any_file));
    let delivery = quiet.node.nets[0].borrow_mut().deliver_whole();
    for n in 0..WRITES {
        let key = format!("ack:{n:03}");
        let set: [&[u8]; 3] = [b"SET", key.as_bytes(), b"1"];
        assert_eq!(encode(&set).len(), SET_BYTES, "the burst's writes share one width");
        writer.send(&mut quiet.node, &set);
    }
    for _ in 0..STEPS_MAX {
        if appended(&quiet) >= before + WRITES as u64 {
            break;
        }
        quiet.step();
    }
    for _ in 0..SETTLE_STEPS {
        quiet.step();
    }
    quiet.node.nets[0].borrow_mut().restore_delivery(delivery);
    let held = quiet.node.driver(0).held_barriers();
    let unheld = format!("VACUOUS frame-barrier-unheld: no frame barrier was held ({held:?})");
    assert!(held.frames() > 0, "{unheld}");
    let staged = appended(&quiet) - before;
    let unstaged = format!("VACUOUS writes-unstaged: {staged} of {WRITES} writes were appended");
    assert!(staged >= WRITES as u64, "{unstaged}");
    let acked = writer.recv(&mut quiet.node);
    assert_eq!(acked, None, "VACUOUS acked-under-hold: an ack was read before the hold lifted");
    let waits = quiet.node.plane(0).ack_waiters_for_sim();
    assert_eq!(
        waits, 1,
        "wait-per-write: {waits} watermark waits are parked for one connection's {WRITES} \
         always writes, where its front's one is"
    );
    quiet.disk.hold_barriers(None);
    let mut acked = 0;
    for _ in 0..STEPS_MAX {
        quiet.step();
        while let Some(reply) = writer.recv(&mut quiet.node) {
            assert_eq!(reply.as_slice(), b"+OK\r\n", "write {acked}'s ack, in pipeline order");
            acked += 1;
        }
        if acked == WRITES {
            break;
        }
    }
    assert_eq!(acked, WRITES, "stranded: {acked} of {WRITES} writes acknowledged after the hold");
    let left = quiet.node.plane(0).ack_waiters_for_sim();
    assert_eq!(left, 0, "{left} watermark waits are left parked after every ack");
}
