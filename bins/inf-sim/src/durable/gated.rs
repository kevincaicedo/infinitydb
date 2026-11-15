//! A tiered `always` verdict keeps its place across a cold read (ADR-0177
//! D6): the owner's pump takes the verdict's slot before the write's first
//! effect, suspends on the cold read and fills it after, while FABRIC-IN
//! diverts the origin's next durable write behind it. Two cells, X the
//! origin of every write and Y the owner of every key; the oracles read the
//! clients' bytes, the device's held reads and barriers and the census rows
//! of Y's deques are their engagement. The `m0-capacity` gated legs hold the
//! other inputs at the shipped topology's simulator.

use std::path::Path;

use inf_store::SlotRouter;

use super::MiniClient;
use super::park::{COLD_KEYS_MAX, COLD_SET_BATCH, Quiet, STEPS_MAX, cold_key, info_number};

/// The origin and the owner.
const X: usize = 0;
const Y: usize = 1;
/// A mesh of 8 data credits a peer, the gated legs' own.
const SMALL_MESH: inf_fabric::MeshConfig = crate::harness::scenario_mesh(1024, 8);
/// Steps a held phase runs before its clauses read it, so every op it sent
/// reached the owner and waits there.
const SETTLE_STEPS: u32 = 64;

/// Holds every barrier, and every read, on every file.
fn any_file(_: &Path) -> bool {
    true
}

/// `count` names cell `owner` owns on a two-cell node, after `stem`.
fn names_on(owner: usize, stem: &str, count: usize) -> Vec<Vec<u8>> {
    let router = SlotRouter::new_contiguous(2);
    (0..)
        .map(|n| format!("{stem}:{n}").into_bytes())
        .filter(|name| usize::from(router.cell_of(SlotRouter::slot_of(name)).0) == owner)
        .take(count)
        .collect()
}

/// The first of the cold keys `0..written` that cell `owner` owns.
fn cold_key_on(owner: usize, written: usize) -> Vec<u8> {
    let router = SlotRouter::new_contiguous(2);
    (0..written)
        .map(cold_key)
        .find(|name| usize::from(router.cell_of(SlotRouter::slot_of(name)).0) == owner)
        .expect("a cold key on the owner")
}

/// One `INFO tripwires` field of `cell`, read over the wire.
fn tripwire(quiet: &mut Quiet, cell: usize, field: &str) -> u64 {
    let mut client = MiniClient::connect(&mut quiet.node, cell);
    let info = quiet.call(&mut client, &[b"INFO", b"tripwires"], "INFO tripwires");
    client.close(&mut quiet.node);
    info_number(&info, field)
}

/// A census row's high water on cell `cell`.
fn high_water(quiet: &Quiet, cell: usize, row: &str) -> u32 {
    quiet.node.plane(cell).cap_census().row(row).map_or(0, |row| row.high_water)
}

/// `CREATE` of a flat `always` namespace, from a client of X.
fn create_always(quiet: &mut Quiet, name: &[u8]) {
    let mut setup = MiniClient::connect(&mut quiet.node, X);
    let create: &[&[u8]] = &[b"INF.NS", b"CREATE", name, b"MODE", b"durable", b"FSYNC", b"always"];
    assert_eq!(quiet.call(&mut setup, create, "the always CREATE"), b"+OK\r\n");
    setup.close(&mut quiet.node);
}

/// A client of `cell` in namespace `name`.
fn client_in(quiet: &mut Quiet, cell: usize, name: &[u8]) -> MiniClient {
    let mut client = MiniClient::connect(&mut quiet.node, cell);
    assert_eq!(quiet.call(&mut client, &[b"INF.NS", b"USE", name], "USE"), b"+OK\r\n");
    client
}

/// Steps until each of `clients` has read one reply, at most `STEPS_MAX`
/// steps: the replies, or `None` for each still waiting.
fn replies(quiet: &mut Quiet, clients: &mut [MiniClient]) -> Vec<Option<Vec<u8>>> {
    let mut read: Vec<Option<Vec<u8>>> = vec![None; clients.len()];
    for _ in 0..STEPS_MAX {
        for (client, reply) in clients.iter_mut().zip(read.iter_mut()) {
            if reply.is_none() {
                *reply = client.recv(&mut quiet.node);
            }
        }
        if read.iter().all(Option::is_some) {
            break;
        }
        quiet.step();
    }
    read
}

/// The deques' end state on a quiet node: no byte parked on Y, and no op
/// refused at a full deque on either cell.
fn quiet_deques(quiet: &mut Quiet) {
    assert_eq!(tripwire(quiet, Y, "fabric_parked_bytes"), 0, "parked-bytes-kept on Y");
    for cell in [X, Y] {
        assert_eq!(tripwire(quiet, cell, "fabric_parked_refused"), 0, "parked-refused on {cell}");
    }
}

/// A tiered `always` write suspended on a cold read between its placeholder
/// and its verdict, a flat `always` write of the same origin behind it: Y
/// writes a tiered namespace past its memory budget until its oldest keys are
/// cold; with the device holding every read and barrier, a client of X
/// `APPEND`s to a cold key Y owns, and once Y's read is held another client
/// of X `SET`s a key of the flat namespace on Y. The `SET` waits behind the
/// suspended `APPEND` on X's pump (the divert rule), and both verdicts wait
/// for the fsync; then the reads, then the barriers, are released and both
/// clients read their replies. Engagement, each else its `VACUOUS` line: Y
/// held a tier read, Y's apply deque for X held the `SET`, and its gated
/// deque held both verdicts. Planted (`inf_canary_ns_apply_skips_divert`),
/// the `SET` runs inline while the `APPEND` holds its placeholder, its
/// verdict lands behind that placeholder, and the `APPEND`'s fill stops the
/// cell: `a filled placeholder is the back of its origin's deque`.
#[test]
fn a_tiered_verdict_keeps_its_place_across_a_cold_read() {
    let mut quiet = Quiet::boot_on(0xC0FFEE, SMALL_MESH, 2, 0, Y as u16);
    let mut setup = MiniClient::connect(&mut quiet.node, Y);
    let create: &[&[u8]] = &[
        b"INF.NS",
        b"CREATE",
        b"cold",
        b"MODE",
        b"durable",
        b"FSYNC",
        b"always",
        b"MEM-BUDGET",
        b"3mb",
        b"MUTABLE-FRACTION",
        b"100",
        b"MAINTAIN-SLICE",
        b"1mb",
        b"TIER-IO-MODE",
        b"buffered",
    ];
    assert_eq!(quiet.call(&mut setup, create, "the tiered CREATE"), b"+OK\r\n");
    assert_eq!(quiet.call(&mut setup, &[b"INF.NS", b"USE", b"cold"], "USE"), b"+OK\r\n");
    let mut written = 0;
    loop {
        assert!(written < COLD_KEYS_MAX, "{written} keys made no cold working set");
        quiet.set_batch(&mut setup, written..written + COLD_SET_BATCH);
        written += COLD_SET_BATCH;
        let tiering = quiet.call(&mut setup, &[b"INFO", b"tiering"], "INFO tiering");
        if info_number(&tiering, "tiering_flush_confirmed_bytes") > 3 << 20
            && info_number(&tiering, "tiering_region_decommit_pages") > 0
        {
            break;
        }
    }
    setup.close(&mut quiet.node);
    create_always(&mut quiet, b"flat");
    let mut appender = client_in(&mut quiet, X, b"cold");
    let mut setter = client_in(&mut quiet, X, b"flat");
    let cold = cold_key_on(Y, written);
    let flat = names_on(Y, "flat", 1).remove(0);
    quiet.disk.hold_barriers(Some(any_file));
    quiet.disk.hold_reads(Some(super::is_tier_file));
    appender.send(&mut quiet.node, &[b"APPEND", &cold, b"+"]);
    for _ in 0..STEPS_MAX {
        if quiet.node.driver(Y).held_reads() > 0 {
            break;
        }
        quiet.step();
    }
    assert!(quiet.node.driver(Y).held_reads() > 0, "VACUOUS read-unheld: Y held no tier read");
    setter.send(&mut quiet.node, &[b"SET", &flat, b"1"]);
    for _ in 0..SETTLE_STEPS {
        quiet.step();
    }
    let early = (appender.recv(&mut quiet.node), setter.recv(&mut quiet.node));
    assert_eq!(early, (None, None), "acked-under-hold: a reply was read before Y's fsync");
    quiet.disk.hold_reads(None);
    for _ in 0..SETTLE_STEPS {
        quiet.step();
    }
    let behind = high_water(&quiet, Y, "ns-applies");
    assert!(behind > 0, "VACUOUS write-not-behind: Y's apply deque for X held nothing");
    let verdicts = high_water(&quiet, Y, "gated");
    assert!(verdicts >= 2, "VACUOUS verdicts-unparked: Y's gated row read {verdicts}");
    quiet.disk.hold_barriers(None);
    let mut clients = [appender, setter];
    let read = replies(&mut quiet, &mut clients);
    let appended = read[0].as_deref().map(|reply| reply.first() == Some(&b':'));
    assert_eq!(appended, Some(true), "the APPEND's reply: {:?}", read[0]);
    assert_eq!(read[1].as_deref(), Some(&b"+OK\r\n"[..]), "the SET's reply");
    for _ in 0..SETTLE_STEPS {
        quiet.step();
    }
    quiet_deques(&mut quiet);
}
