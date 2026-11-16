//! A fan wider than its connection task's gate units runs in windows (ADR-0177 D4): a send that
//! finds every unit out settles the fan's oldest leg first, folding or keeping its outcome. Each
//! test drives two cells of one node, cell 1 owning every key and channel of the command and
//! answering each fabric drain newest first, so cell 0 reads every window's replies out of send
//! order. The command crosses its window with a contribution from each leg, and the reply is
//! checked against what the test wrote through cell 1's own clients: a fold that loses a settled
//! leg's count, an element out of argv order or a pair left unwritten reads red.

use super::wire_rig::{Rig, frame};
use super::*;

/// Scheduler rounds an exchange may take before the test calls it stalled.
const ROUNDS_MAX: usize = 256;

/// The client on cell 0 whose commands fan out to cell 1.
const CLIENT: RawFd = 100;

/// A client on cell 1 that writes and reads the keys locally.
const LOCAL: RawFd = 101;

/// The legs a connection's task may have out at once: its gate units.
const WINDOW: usize = crate::limits::GATE_UNITS_PER_CONN_TASK as usize;

/// A node of two cells, a client on each, cell 1 answering newest first.
fn node() -> Rig {
    let mut rig = Rig::new(2);
    rig.accept(0, CLIENT);
    rig.accept(1, LOCAL);
    rig.reverse_replies(1, true);
    rig
}

/// Sends `request` on `fd` of `cell` and runs the node until the bytes it writes there reach
/// `want`'s length; they must be `want`, and nothing more follows.
fn exchange(rig: &mut Rig, cell: usize, fd: RawFd, request: &[u8], want: &[u8]) {
    let before = rig.written(cell, fd).len();
    rig.send(cell, fd, request);
    let mut rounds = 0;
    while rig.written(cell, fd).len() < before + want.len() {
        assert!(rounds < ROUNDS_MAX, "cell {cell} answered no full reply in {ROUNDS_MAX} rounds");
        rig.rounds(1);
        rounds += 1;
    }
    rig.rounds(2);
    let written = rig.written(cell, fd);
    assert_eq!(
        String::from_utf8_lossy(&written[before..]),
        String::from_utf8_lossy(want),
        "cell {cell}'s reply"
    );
}

/// `argv` sent from `fd` of `cell`, its reply `want`.
fn command(rig: &mut Rig, cell: usize, fd: RawFd, argv: &[&[u8]], want: &[u8]) {
    exchange(rig, cell, fd, &frame(argv), want);
}

/// Each `(key, value)` set through cell 1's local client, one pipeline.
fn set_on_owner(rig: &mut Rig, pairs: &[(&[u8], &[u8])]) {
    let mut request = Vec::new();
    for (key, value) in pairs {
        request.extend(frame(&[b"SET", key, value]));
    }
    exchange(rig, 1, LOCAL, &request, &b"+OK\r\n".repeat(pairs.len()));
}

/// A RESP2 bulk string.
fn bulk(bytes: &[u8]) -> Vec<u8> {
    let mut out = format!("${}\r\n", bytes.len()).into_bytes();
    out.extend_from_slice(bytes);
    out.extend_from_slice(b"\r\n");
    out
}

/// Asserts that cell 1 answered a whole window of cell 0's legs newest first since the last
/// read: the command crossed its window, and its replies came back out of send order.
fn assert_window_reversed(rig: &mut Rig, what: &str) {
    let reversed = rig.take_reversed_batch_max(1);
    assert_eq!(reversed, WINDOW, "engagement: {what} sent no full window of legs at once");
}

/// `EXISTS` and `DEL` of 100 keys cell 1 holds count every key, across four windows.
#[test]
fn a_counted_fan_past_its_window_counts_every_remote_key() {
    const KEYS: usize = 100;
    let mut rig = node();
    let keys = rig.keys_on(1, "counted", KEYS);
    let pairs: Vec<(&[u8], &[u8])> = keys.iter().map(|key| (key.as_slice(), &b"x"[..])).collect();
    set_on_owner(&mut rig, &pairs);
    let mut exists: Vec<&[u8]> = vec![b"EXISTS"];
    exists.extend(keys.iter().map(Vec::as_slice));
    command(&mut rig, 0, CLIENT, &exists, b":100\r\n");
    assert_window_reversed(&mut rig, "EXISTS");
    let mut del: Vec<&[u8]> = vec![b"DEL"];
    del.extend(keys.iter().map(Vec::as_slice));
    command(&mut rig, 0, CLIENT, &del, b":100\r\n");
    assert_window_reversed(&mut rig, "DEL");
    command(&mut rig, 0, CLIENT, &exists, b":0\r\n");
}

/// `MGET` of 100 keys cell 1 owns, every third one present, answers in argv order.
#[test]
fn a_gather_past_its_window_answers_in_argv_order() {
    const KEYS: usize = 100;
    let mut rig = node();
    let keys = rig.keys_on(1, "gather", KEYS);
    let values: Vec<Vec<u8>> = (0..KEYS).map(|n| format!("v{n}").into_bytes()).collect();
    let present: Vec<(&[u8], &[u8])> = (0..KEYS)
        .filter(|n| n % 3 == 0)
        .map(|n| (keys[n].as_slice(), values[n].as_slice()))
        .collect();
    set_on_owner(&mut rig, &present);
    let mut want = format!("*{KEYS}\r\n").into_bytes();
    for (n, value) in values.iter().enumerate() {
        if n % 3 == 0 {
            want.extend(bulk(value));
        } else {
            want.extend_from_slice(b"$-1\r\n");
        }
    }
    let mut mget: Vec<&[u8]> = vec![b"MGET"];
    mget.extend(keys.iter().map(Vec::as_slice));
    command(&mut rig, 0, CLIENT, &mget, &want);
    assert_window_reversed(&mut rig, "MGET");
}

/// `MSET` of 50 pairs cell 1 owns answers `+OK` with every pair written, as cell 1 reads them.
#[test]
fn an_all_ok_fan_past_its_window_writes_every_pair() {
    const PAIRS: usize = 50;
    let mut rig = node();
    let keys = rig.keys_on(1, "allok", PAIRS);
    let values: Vec<Vec<u8>> = (0..PAIRS).map(|n| format!("m{n}").into_bytes()).collect();
    let mut mset: Vec<&[u8]> = vec![b"MSET"];
    for (key, value) in keys.iter().zip(&values) {
        mset.push(key);
        mset.push(value);
    }
    command(&mut rig, 0, CLIENT, &mset, b"+OK\r\n");
    assert_window_reversed(&mut rig, "MSET");
    let mut want = format!("*{PAIRS}\r\n").into_bytes();
    for value in &values {
        want.extend(bulk(value));
    }
    let mut mget: Vec<&[u8]> = vec![b"MGET"];
    mget.extend(keys.iter().map(Vec::as_slice));
    command(&mut rig, 1, LOCAL, &mget, &want);
}

/// `PUBSUB NUMSUB` of 40 channels cell 1 owns answers each channel's subscribers in argv order:
/// a client on cell 1 subscribes to every channel and a client on cell 0 to the even ones, so a
/// channel's count is the cells its subscribers are on.
#[test]
fn an_ordered_fan_past_its_window_reads_each_owners_count() {
    const CHANNELS: usize = 40;
    const SUBSCRIBER: RawFd = 102;
    let mut rig = node();
    rig.accept(0, SUBSCRIBER);
    let channels = rig.keys_on(1, "numsub", CHANNELS);
    for (cell, fd, step) in [(1, LOCAL, 1), (0, SUBSCRIBER, 2)] {
        let mut subscribe: Vec<&[u8]> = vec![b"SUBSCRIBE"];
        subscribe.extend(channels.iter().step_by(step).map(Vec::as_slice));
        let mut confirmations = Vec::new();
        for (n, channel) in channels.iter().step_by(step).enumerate() {
            confirmations.extend_from_slice(b"*3\r\n");
            confirmations.extend(bulk(b"subscribe"));
            confirmations.extend(bulk(channel));
            confirmations.extend(format!(":{}\r\n", n + 1).into_bytes());
        }
        command(&mut rig, cell, fd, &subscribe, &confirmations);
    }
    let mut want = format!("*{}\r\n", 2 * CHANNELS).into_bytes();
    for (n, channel) in channels.iter().enumerate() {
        want.extend(bulk(channel));
        want.extend(format!(":{}\r\n", if n % 2 == 0 { 2 } else { 1 }).into_bytes());
    }
    let mut numsub: Vec<&[u8]> = vec![b"PUBSUB", b"NUMSUB"];
    numsub.extend(channels.iter().map(Vec::as_slice));
    rig.take_reversed_batch_max(1);
    command(&mut rig, 0, CLIENT, &numsub, &want);
    assert_window_reversed(&mut rig, "PUBSUB NUMSUB");
}

/// The cell's recorded hops: the fabric gate's round-trip samples.
fn hops(rig: &Rig, cell: usize) -> u64 {
    rig.cells[cell].plane.shared.gate.borrow().round_trips().count()
}

/// The hop RTT gate's samples are the hops the peer answers as it applies: a remote `DEL` of 40
/// keys adds 40 on the sending cell, past its window and answered newest first; a remote
/// `PUBLISH`'s `INF.PUB`, whose reply waits on the owner's fan, and a namespace's `ApplyNs`,
/// whose reply can wait on the owner's fsync, add none.
#[test]
fn a_cell_samples_each_applied_hop_and_no_deferred_one() {
    const KEYS: usize = 40;
    let namespace = inf_store::NsSpec {
        id: NsId(inf_store::FIRST_NAMED_NS_ID),
        name: b"hops".to_vec(),
        mode: NsMode::Memory,
        fsync: None,
        policy: None,
        maxmemory: None,
        tier: None,
    };
    let mut rig = Rig::with_namespaces(2, &[namespace]);
    rig.accept(0, CLIENT);
    rig.reverse_replies(1, true);
    let keys = rig.keys_on(1, "hops", KEYS);
    let mut del: Vec<&[u8]> = vec![b"DEL"];
    del.extend(keys.iter().map(Vec::as_slice));
    let before = hops(&rig, 0);
    command(&mut rig, 0, CLIENT, &del, b":0\r\n");
    assert_window_reversed(&mut rig, "DEL");
    assert_eq!(hops(&rig, 0) - before, KEYS as u64, "one sample a remote DEL's leg");
    let channel = rig.key_on(1, "hops-channel");
    let before = hops(&rig, 0);
    command(&mut rig, 0, CLIENT, &[b"PUBLISH", &channel, b"m"], b":0\r\n");
    assert_eq!(hops(&rig, 0), before, "INF.PUB is no hop sample");
    command(&mut rig, 0, CLIENT, &[b"INF.NS", b"USE", b"hops"], b"+OK\r\n");
    let key = rig.key_on(1, "hops-ns");
    command(&mut rig, 0, CLIENT, &[b"SET", &key, b"v"], b"+OK\r\n");
    assert_eq!(hops(&rig, 0), before, "an ApplyNs is no hop sample");
    let owner = rig.cells[1].plane.shared.store.borrow();
    let held = owner.ns_store(NsId(inf_store::FIRST_NAMED_NS_ID)).map_or(0, |store| store.len());
    assert_eq!(held, 1, "engagement: the namespaced SET applied on the key's owner");
}
