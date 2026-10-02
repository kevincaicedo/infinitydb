//! `LASTSAVE` and `rdb_last_save_time` at the loop tier (ADR-0159 A1.4):
//! one value per cell, in the one schedule where a `WAIT CELL k` confirms
//! ahead of the cell's sweep.

use inf_foundation::CellId;
use inf_foundation::time::Nanos;
use inf_server::CkptTarget;

use super::MiniClient;
use super::park::{Quiet, STEPS_MAX};

/// The integer of a RESP `:<n>\r\n` reply.
fn integer(reply: &[u8]) -> u64 {
    let text = core::str::from_utf8(reply).expect("utf-8 reply");
    text.strip_prefix(':')
        .and_then(|rest| rest.trim_end().parse().ok())
        .unwrap_or_else(|| panic!("not an integer reply: {text:?}"))
}

/// `rdb_last_save_time` of an `INFO persistence` reply.
fn rdb_last_save_time(reply: &[u8]) -> u64 {
    let text = core::str::from_utf8(reply).expect("utf-8 reply");
    text.lines()
        .find_map(|line| line.strip_prefix("rdb_last_save_time:"))
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or_else(|| panic!("no rdb_last_save_time in {text:?}"))
}

/// `INF.CKPT CELL 1 WAIT`, `LASTSAVE` and `INFO` pipelined on cell 0 of a
/// 2-cell node, under the schedule that separates the two sources: cell 0's
/// sweep wakes the `WAIT` on cell 0's own publication, and cell 1 publishes
/// two seconds later, before cell 0 runs again. Cell 0's next iteration
/// confirms the `WAIT` against slot 1 and answers all three commands before
/// its MAINTAIN sweeps, so its observation still holds the older second.
/// Both surfaces must answer slot 1's publication second, which the test
/// reads from the board itself.
#[test]
fn lastsave_and_info_answer_one_second_when_a_wait_confirms_ahead_of_the_sweep() {
    let mut quiet = Quiet::boot(2, super::DurableScenario::SPIN_ITERS, 0);
    let mut client = MiniClient::connect(&mut quiet.node, 0);
    // Cell 1 cannot publish while cell 0 takes the three commands.
    quiet.node.frozen = Some((1, u64::MAX));
    client.send(&mut quiet.node, &[b"INF.CKPT", b"CELL", b"1", b"WAIT"]);
    client.send(&mut quiet.node, &[b"LASTSAVE"]);
    client.send(&mut quiet.node, &[b"INFO", b"persistence"]);
    // The WAIT parks, and every byte behind it is delivered and queued.
    for _ in 0..256 {
        quiet.step();
    }
    assert_eq!(quiet.waiters(), 1, "the WAIT is parked on cell 0");
    assert_eq!(client.recv(&mut quiet.node), None, "nothing answers before slot 1 publishes");

    // Cell 0 publishes for a host request; the sweep that sees it wakes
    // the WAIT. Cell 0 stops there, before the woken pump runs.
    let own_before = quiet.published(0);
    quiet.node.ckpt_host.request(CkptTarget::Cell(CellId(0))).expect("a host unit");
    let mut steps = 0;
    while quiet.waiters() > 0 {
        assert!(steps < STEPS_MAX, "cell 0's sweep never woke the WAIT");
        quiet.step();
        steps += 1;
    }
    assert_ne!(quiet.published(0), own_before, "cell 0 published");
    let own_unix_s = quiet.node.control.ckpt_board().slot(0).last_unix_ms() / 1000;

    // Cell 1 publishes the epoch the WAIT fenced, two seconds later.
    quiet.node.frozen = Some((0, u64::MAX));
    quiet.clock.advance(Nanos(2_000_000_000));
    let fenced = quiet.node.control.ckpt_board().requested(1);
    assert_ne!(fenced, 0, "the WAIT requested slot 1");
    let mut steps = 0;
    while quiet.published(1) < fenced {
        assert!(steps < STEPS_MAX, "cell 1 never published the fenced epoch");
        quiet.step();
        steps += 1;
    }
    let fenced_unix_s = quiet.node.control.ckpt_board().slot(1).last_unix_ms() / 1000;
    assert!(fenced_unix_s > own_unix_s, "VACUOUS: one second holds both publications");

    // Cell 0 runs once: its pump confirms the WAIT and dispatches the two
    // commands queued behind it, ahead of that iteration's MAINTAIN. The
    // two short replies leave whole in the next iteration's submit; the
    // INFO reply takes a few more sends.
    quiet.node.frozen = None;
    quiet.step();
    quiet.step();
    let mut replies: Vec<Vec<u8>> = core::iter::from_fn(|| client.recv(&mut quiet.node)).collect();
    assert_eq!(replies.len(), 2, "VACUOUS: the WAIT did not confirm in cell 0's first iteration");
    for _ in 0..64 {
        if let Some(reply) = client.recv(&mut quiet.node) {
            replies.push(reply);
            break;
        }
        quiet.step();
    }
    assert_eq!(replies.len(), 3, "INFO never answered");
    assert_eq!(replies[0], b"+OK\r\n", "the WAIT answers");
    let lastsave = integer(&replies[1]);
    let info = rdb_last_save_time(&replies[2]);
    assert!(lastsave >= fenced_unix_s, "LASTSAVE {lastsave} trails slot 1's {fenced_unix_s}");
    assert_eq!(info, lastsave, "rdb_last_save_time and LASTSAVE differ on one cell");
}
