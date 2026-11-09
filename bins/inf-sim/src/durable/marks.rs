//! The harness's marks on the simulated disk: `Node::step` marks the disk
//! before each cell's iteration, so every call the disk logs names the cell
//! that made it and the injected time of its step. An oracle that spaces
//! checkpoints or matches attempts reads these entries, never the cell's
//! own counters.

use std::path::Path;

use inf_foundation::CellId;
use inf_foundation::time::Clock;
use inf_log::fs::sim::{CallResult, DiskCall, DiskPhase, LoggedCall};

use super::MiniClient;
use super::park::{Quiet, STEPS_MAX};

/// The cell whose shard directory holds `path` (`…/shard-N/…`).
fn shard_of(path: &Path) -> Option<u16> {
    path.components()
        .find_map(|part| part.as_os_str().to_str()?.strip_prefix("shard-")?.parse().ok())
}

/// Sends `argv` on `client` and steps the node until its reply.
fn call(quiet: &mut Quiet, client: &mut MiniClient, argv: &[&[u8]]) -> Vec<u8> {
    client.send(&mut quiet.node, argv);
    for _ in 0..STEPS_MAX {
        quiet.step();
        if let Some(reply) = client.recv(&mut quiet.node) {
            return reply;
        }
    }
    panic!("no reply to {argv:?} in {STEPS_MAX} steps");
}

/// The position in `calls` of the first call of `cell` that `wanted` accepts.
fn first_of(calls: &[LoggedCall], cell: u16, wanted: impl Fn(&LoggedCall) -> bool) -> usize {
    calls
        .iter()
        .position(|call| shard_of(&call.path) == Some(cell) && wanted(call))
        .unwrap_or_else(|| panic!("cell {cell}: no such call in {calls:#?}"))
}

/// `INF.CKPT WAIT` on a two-cell node: each cell's staging create and its
/// `MANIFEST` rename are logged, the create first, and every call the
/// checkpoint made carries the mark of the cell whose shard it names, at an
/// injected time inside the steps the test ran.
#[test]
fn each_checkpoint_call_carries_the_mark_of_the_cell_that_made_it() {
    let mut quiet = Quiet::boot(2, 0, 0);
    let first = quiet.node.disk.logged_calls(0).len();
    let begun = quiet.clock.now();
    let mut client = MiniClient::connect(&mut quiet.node, 0);
    let reply = call(&mut quiet, &mut client, &[b"INF.CKPT", b"WAIT"]);
    assert_eq!(reply, b"+OK\r\n", "INF.CKPT WAIT");
    let ended = quiet.clock.now();
    let calls = quiet.node.disk.logged_calls(first);
    for cell in 0..2 {
        let created = first_of(&calls, cell, |call| {
            call.call == DiskCall::CheckpointCreate { left_file: true }
                && call.result == CallResult::Ok
        });
        let renamed = first_of(&calls, cell, |call| {
            call.call == DiskCall::ManifestRename && call.result == CallResult::Ok
        });
        assert!(created < renamed, "cell {cell}: the create precedes the rename: {calls:#?}");
    }
    for call in &calls {
        let mark = call.mark.unwrap_or_else(|| panic!("an unmarked call: {call:?}"));
        let cell = shard_of(&call.path).unwrap_or_else(|| panic!("no shard: {call:?}"));
        assert_eq!(mark.phase, DiskPhase::Cell(CellId(cell)), "made by its own cell: {call:?}");
        assert!(begun <= mark.at && mark.at <= ended, "at a step the test ran: {call:?}");
    }
    assert_eq!(quiet.node.disk.calls_unlogged(), 0);
}
