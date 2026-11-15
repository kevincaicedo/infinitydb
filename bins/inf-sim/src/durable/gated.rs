//! An owner's `always` verdicts for a peer (ADR-0177 D3, D6): each waits in
//! its origin's gated deque, published only once the owner's fsync watermark
//! covers it, in its origin's `seq` order. Three inputs on two cells, X the
//! origin of every write and Y the owner of every key: a peer's credits' worth
//! of writes under a held fsync; a tiered write suspended on a cold read
//! between its placeholder and its verdict, with a flat write of the same
//! origin behind it; and a verdict that is an error reply, with a write after
//! it. The oracles read the clients' bytes; the device's held barriers and
//! reads, the census rows of Y's deques and Y's `INFO tripwires` are their
//! engagement and the deques' end state.

use std::path::Path;

use inf_store::SlotRouter;

use super::MiniClient;
use super::park::{COLD_KEYS_MAX, COLD_SET_BATCH, Quiet, STEPS_MAX, cold_key, info_number};

/// The origin and the owner.
const X: usize = 0;
const Y: usize = 1;
/// A mesh of 8 data credits a peer: a general send keeps one free for the
/// publish fan, so an origin holds 7 un-replied ops toward a peer.
const SMALL_MESH: inf_fabric::MeshConfig = crate::harness::scenario_mesh(1024, 8);
const UNREPLIED_MAX: usize = 7;
/// Steps a held phase runs before its clauses read it, so every op it sent
/// reached the owner and waits there.
const SETTLE_STEPS: u32 = 64;
/// The seeds the held-fsync input runs at: its clause holds whatever the
/// schedule, and the planted build breaks it at each.
const SEEDS: [u64; 4] = [0xC0FFEE, 0x1, 0x2, 0x3];

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

/// A peer's credits' worth of `always` writes from one origin while the
/// owner's fsync is held: 7 clients of X, one `SET` each to a key Y owns,
/// sent at one step and delivered whole. No client reads an ack before the
/// hold lifts (`acked-under-hold`, the oracle, read at every seed before it
/// judges); every client reads `+OK`. Engagement, each else its `VACUOUS`
/// line: the device held a frame barrier, and Y's gated deque for X held the
/// verdicts. Planted (`inf_canary_gated_ignores_watermark`), the `gated` task
/// publishes every filled verdict, and the acks are read under the hold, at
/// every seed.
#[test]
fn a_peers_always_verdicts_wait_for_the_owners_fsync() {
    let mut early = Vec::new();
    for seed in SEEDS {
        let mut quiet = Quiet::boot_on(seed, SMALL_MESH, 2, 0, Y as u16);
        create_always(&mut quiet, b"acks");
        let mut writers: Vec<MiniClient> =
            (0..UNREPLIED_MAX).map(|_| client_in(&mut quiet, X, b"acks")).collect();
        quiet.disk.hold_barriers(Some(any_file));
        let delivery = quiet.node.nets[X].borrow_mut().deliver_whole();
        for (writer, name) in writers.iter_mut().zip(names_on(Y, "acks", UNREPLIED_MAX)) {
            writer.send(&mut quiet.node, &[b"SET", &name, b"1"]);
        }
        for _ in 0..SETTLE_STEPS {
            quiet.step();
        }
        quiet.node.nets[X].borrow_mut().restore_delivery(delivery);
        let held = quiet.node.driver(Y).held_barriers();
        assert!(held.frames() > 0, "VACUOUS frame-barrier-unheld at seed {seed:#x}: {held:?}");
        let parked = high_water(&quiet, Y, "gated");
        assert!(parked > 0, "VACUOUS verdicts-unparked at seed {seed:#x}: Y's gated row read 0");
        let mut waiting = Vec::new();
        for (index, mut writer) in writers.into_iter().enumerate() {
            match writer.recv(&mut quiet.node) {
                Some(reply) => early.push(format!("seed {seed:#x} writer {index}: {reply:?}")),
                None => waiting.push(writer),
            }
        }
        quiet.disk.hold_barriers(None);
        for (index, reply) in replies(&mut quiet, &mut waiting).into_iter().enumerate() {
            assert_eq!(reply.as_deref(), Some(&b"+OK\r\n"[..]), "writer {index} at seed {seed:#x}");
        }
        for _ in 0..SETTLE_STEPS {
            quiet.step();
        }
        quiet_deques(&mut quiet);
    }
    assert!(
        early.is_empty(),
        "acked-under-hold: {} ack(s) read before the owner's fsync: {early:?}",
        early.len()
    );
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

/// A verdict that is an error reply gives its placeholder back: a client of
/// X `INCR`s a key Y holds as text, answered at once with Redis's error, and
/// another client of X then `SET`s a key on Y, answered once Y's fsync
/// covers it. Planted (`inf_canary_gated_error_keeps_placeholder`), the
/// error leaves its placeholder at the front of Y's gated deque for X, and
/// the later write's verdict waits behind it for good
/// (`verdict-behind-an-error`).
#[test]
fn an_error_verdict_gives_its_placeholder_back() {
    let mut quiet = Quiet::boot_on(0xC0FFEE, SMALL_MESH, 2, 0, Y as u16);
    create_always(&mut quiet, b"errs");
    let mut owner = client_in(&mut quiet, Y, b"errs");
    let names = names_on(Y, "errs", 2);
    let set: &[&[u8]] = &[b"SET", &names[0], b"text"];
    assert_eq!(quiet.call(&mut owner, set, "the owner's SET"), b"+OK\r\n");
    let mut incr = client_in(&mut quiet, X, b"errs");
    let error = quiet.call(&mut incr, &[b"INCR", &names[0]], "the INCR");
    assert_eq!(error, b"-ERR value is not an integer or out of range\r\n");
    let mut after = [client_in(&mut quiet, X, b"errs")];
    after[0].send(&mut quiet.node, &[b"SET", &names[1], b"1"]);
    let read = replies(&mut quiet, &mut after);
    assert_eq!(
        read[0].as_deref(),
        Some(&b"+OK\r\n"[..]),
        "verdict-behind-an-error: a write after an error verdict was not answered"
    );
    for _ in 0..SETTLE_STEPS {
        quiet.step();
    }
    quiet_deques(&mut quiet);
}
