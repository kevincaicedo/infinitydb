//! A reply's outcome is read from the reply, never from its first byte
//! (ADR-0190). Each test drives a real program on cell 0 of a two-cell
//! fabric; the peer answers through its own drain and staging, so the
//! outcome crosses the fabric exactly as a node carries it, and the test
//! reads what the client would receive.

use std::future::Future;
use std::pin::pin;
use std::task::{Context, Poll, Waker};

use inf_fabric::{Mesh, MeshConfig};
use inf_foundation::KeySlot;
use inf_foundation::fault::FaultSpec;
use inf_store::{
    AddrClass, Keyspace, LogicalAddr, NsSpec, ShadowProbe, ShadowVerdict, StoreConfig, TierSpec,
    TieredTable,
};

use crate::fault::SHADOW_TWIN_READ_FAIL;

#[cfg(feature = "doc")]
use super::dispatch::gather_json_mget;
use super::dispatch::{dispatch_scatter, program_mset, settle_reply};
use super::publish::program_pubsub;
use super::scatter::{PROGRAM_REPLY_MALFORMED, read_scan_leg};
use super::*;

/// The client-side key every settled reply is written for (no connection
/// is live: a publish stash lookup finds none).
const KEY: ConnKey = ConnKey { slot: 0, generation: 0 };
const ORIGIN: ExecOrigin = ExecOrigin::Conn(0, 0);

struct Cells {
    planes: Vec<ServerPlane<NoopObserver>>,
    /// Per cell, in cell order: the applies its ns-apply pumps took, which
    /// a row that claims a pump arm reads as its engagement.
    pumped: Vec<Cell<usize>>,
}

impl Cells {
    fn new() -> Self {
        let planes: Vec<_> = Mesh::new(2, MeshConfig::default())
            .into_iter()
            .enumerate()
            .map(|(cell, fabric)| {
                let (plane, _serve_mark, _timer_set) = ServerPlane::new(
                    CellId(cell as u16),
                    2,
                    -1,
                    Keyspace::new(StoreConfig::default()),
                    fabric,
                    Rc::new(NodeInfo::try_default().expect("fixture cache allocation")),
                    NoopObserver,
                    false,
                )
                .expect("the server's timer owners fit their cap");
                plane
            })
            .collect();
        let pumped = planes.iter().map(|_| Cell::new(0)).collect();
        Cells { planes, pumped }
    }

    fn origin(&self) -> &Rc<Shared<NoopObserver, StdSegmentFs>> {
        &self.planes[0].shared
    }

    /// A key cell `owner` owns.
    fn key_on(&self, owner: u16) -> Vec<u8> {
        (0..1000)
            .map(|n| format!("reply:{n}").into_bytes())
            .find(|k| self.origin().router.cell_of(SlotRouter::slot_of(k)) == CellId(owner))
            .expect("both owners reachable")
    }

    /// `argv` executed on `cell`'s own store, as a program's local leg.
    fn local(&self, cell: usize, argv: &[&[u8]]) -> Vec<u8> {
        run_local(&self.planes[cell].shared, ORIGIN, Protocol::Resp2, 0, 0, argv).into_bytes()
    }

    /// Polls `program` to completion; between polls every cell drains its
    /// fabric through the plane's own drain, unless `answer` answers an op
    /// itself (a peer completing a leg with a chosen outcome).
    fn drive<T>(
        &self,
        program: impl Future<Output = T>,
        mut answer: impl FnMut(&[&[u8]]) -> Option<Outcome<'static>>,
    ) -> T {
        let mut program = pin!(program);
        let mut cx = Context::from_waker(Waker::noop());
        for _ in 0..32 {
            if let Poll::Ready(reply) = program.as_mut().poll(&mut cx) {
                return reply;
            }
            for plane in &self.planes {
                plane.shared.fabric.borrow_mut().flush();
            }
            for (plane, pumped) in self.planes.iter().zip(&self.pumped) {
                drain(&plane.shared, &mut answer);
                pumped.set(pumped.get() + wake_ns_pumps(&plane.shared));
            }
        }
        panic!("the program exceeded its bounded legs");
    }
}

/// The cell loop's wake of each origin's ns-apply pump after a FABRIC-IN
/// pass, where a tiered apply waits in its origin's FIFO. The harness runs
/// no executor, so a pump must finish in one poll: no leg here suspends,
/// since no cold read reaches a device. Returns the applies the woken
/// pumps took: a pump that finished took its whole queue.
fn wake_ns_pumps(shared: &Rc<Shared<NoopObserver, StdSegmentFs>>) -> usize {
    let (origins, applies) = {
        let (queues, active) = (shared.ns_applies.borrow(), shared.ns_pump_active.borrow());
        let origins: Vec<u16> = (0..shared.cells)
            .filter(|&origin| {
                !queues[usize::from(origin)].is_empty() && !active[usize::from(origin)]
            })
            .collect();
        let applies: usize = origins.iter().map(|&origin| queues[usize::from(origin)].len()).sum();
        (origins, applies)
    };
    let mut cx = Context::from_waker(Waker::noop());
    for origin in origins {
        shared.ns_pump_active.borrow_mut()[usize::from(origin)] = true;
        let pump = pin!(ns_apply_pump(Rc::clone(shared), origin));
        assert!(pump.poll(&mut cx).is_ready(), "the ns-apply pump suspended: no device here");
    }
    applies
}

/// One FABRIC-IN pass of `shared`, then its staged replies shipped.
fn drain(
    shared: &Shared<NoopObserver, StdSegmentFs>,
    answer: &mut impl FnMut(&[&[u8]]) -> Option<Outcome<'static>>,
) {
    let (mut scratch, mut staged, mut pubs, mut gated, mut orphans) =
        (ReplyStream::new(Vec::new()), Vec::new(), Vec::new(), Vec::new(), 0u64);
    let mut answered = Vec::new();
    let now = shared.now.get();
    shared.fabric.borrow_mut().drain(64, |from, op| {
        if let Op::Apply { token, args, .. } = &op
            && let Some(outcome) = answer(args.as_slice())
        {
            answered.push((from, *token, outcome));
            return;
        }
        handle_fabric_op(
            shared,
            now,
            from,
            op,
            &mut scratch,
            &mut staged,
            &mut pubs,
            &mut gated,
            &mut orphans,
        );
    });
    assert!(gated.is_empty() && pubs.is_empty(), "no durable or publish leg in these rows");
    // The cell loop's own shipping of each staged reply (the tag from the
    // reply's outcome).
    let mut fabric = shared.fabric.borrow_mut();
    for (to, token, reply) in staged {
        fabric.reply(to, token, &staged_outcome(scratch.as_bytes(), &reply));
    }
    for (to, token, outcome) in answered {
        fabric.reply(to, token, &outcome);
    }
    fabric.flush();
}

fn no_answer(_: &[&[u8]]) -> Option<Outcome<'static>> {
    None
}

/// A peer's error in RESP3's second encoding (`!`): a leg that reads its
/// outcome from the first byte takes it for a success.
const BLOB: &[u8] = b"!5\r\nERR x\r\n";

/// An error as this build's writer encodes it, so a row reads the same
/// under the blob-error plant as on the plain build.
fn written_error(text: &str) -> Vec<u8> {
    let mut reply = OpenReply::new(Vec::new());
    RespWriter::new(reply.reply(), Protocol::Resp2).error(text);
    reply.finish().into_bytes()
}

/// Under `inf_canary_resp3_blob_error` the peer's error is written in the
/// second encoding; a row whose peer wrote no such error proves nothing.
fn engaged(peer_error: &[u8]) {
    if cfg!(inf_canary_resp3_blob_error) {
        assert_eq!(peer_error.first(), Some(&b'!'), "VACUOUS: the plant wrote no blob error");
    }
}

/// Puts `cell` under a one-byte budget that refuses every write.
fn refuse_writes(cells: &Cells, cell: usize) -> Vec<u8> {
    let budget: &[&[u8]] =
        &[b"CONFIG", b"SET", b"maxmemory", b"1", b"maxmemory-policy", b"noeviction"];
    assert_eq!(cells.local(cell, budget), b"+OK\r\n");
    let refusal = cells.local(cell, &[b"SET", b"probe", b"v"]);
    engaged(&refusal);
    refusal
}

/// A fan leg completed with a fabric refusal (`Err(code)`) is the fan's
/// failure: the fold answers the one refusal text, never `+OK`.
#[test]
fn reply_outcome_mset_refused_leg_is_not_ok() {
    let cells = Cells::new();
    let remote = cells.key_on(1);
    let shared = cells.origin();
    let argv: &[&[u8]] = &[b"MSET", &remote, b"v"];
    let reply = cells.drive(
        async {
            let mut inflight = 0;
            let pending =
                program_mset(shared, ORIGIN, Protocol::Resp2, 0, 0, argv, &mut inflight).await;
            settle_reply(shared, KEY, pending, &mut inflight).await.into_bytes()
        },
        |args| (args[0] == b"SET").then_some(Outcome::Err(ErrCode::OutOfMemory)),
    );
    assert_eq!(reply, written_error(CROSS_CELL_EXECUTION_FAILED));
}

/// A remote pair the peer refuses: the client reads the peer's own error.
#[test]
fn reply_outcome_mset_remote_error_leg_is_the_reply() {
    let cells = Cells::new();
    let remote = cells.key_on(1);
    let refusal = refuse_writes(&cells, 1);
    let shared = cells.origin();
    let argv: &[&[u8]] = &[b"MSET", &remote, b"v"];
    let reply = cells.drive(
        async {
            let mut inflight = 0;
            let pending =
                program_mset(shared, ORIGIN, Protocol::Resp2, 0, 0, argv, &mut inflight).await;
            settle_reply(shared, KEY, pending, &mut inflight).await.into_bytes()
        },
        no_answer,
    );
    assert_eq!(reply, refusal);
}

/// A counted leg that answers an error: the fold's reply is that error,
/// not the fail-closed internal one a non-count success leg earns.
#[test]
fn reply_outcome_dbsize_drain_error_is_the_reply() {
    let cells = Cells::new();
    let argv: &[&[u8]] = &[b"DBSIZE", b"extra"];
    let expected = cells.local(1, argv);
    engaged(&expected);
    let shared = cells.origin();
    let reply = cells.drive(
        async {
            let mut inflight = 1;
            let waiter =
                send_apply(shared, CellId(1), ApplyOrigin::Client, Protocol::Resp2, 0, argv)
                    .await
                    .expect("the leg is sent");
            let counted = PendingReply::Counted {
                waiters: vec![waiter],
                acc: 0,
                proto: Protocol::Resp2,
                refusal: None,
            };
            settle_reply(shared, KEY, counted, &mut inflight).await.into_bytes()
        },
        no_answer,
    );
    assert_eq!(reply, expected);
}

/// The tiered namespace of the drain row, registered and materialized on
/// both cells, as the catalog is on every cell of a node.
const TIERED: NsId = NsId(16);

fn tiered_namespace(cells: &Cells) {
    for plane in &cells.planes {
        let spec = NsSpec {
            id: TIERED,
            name: b"tiered".to_vec(),
            mode: NsMode::Durable,
            fsync: None,
            policy: None,
            maxmemory: None,
            tier: Some(TierSpec::for_budget(64 << 20)),
        };
        plane.shared.store.borrow_mut().ns_create(spec).expect("a fresh tiered namespace");
    }
}

/// Makes `key`'s record cold in `shared`'s tiered table, then overwrites
/// it through the shadow path, the store's own sequence: the flush moves
/// the watermarks past the record (no tier file exists, and none is read:
/// the fault point fires first), and the write opens one ticket whose
/// twin is unread. Returns the ticket's hash and cold address with the
/// twin's image, which verifies the ticket as a twin read would.
fn open_unread_twin(
    shared: &Shared<NoopObserver, StdSegmentFs>,
    key: &[u8],
) -> (u64, LogicalAddr, Vec<u8>) {
    let mut store = shared.store.borrow_mut();
    let table = store.tiered_store_mut(TIERED).expect("materialized");
    table.set_shadow_enabled(true);
    let hash = table.hash_key(key);
    let cold = table.insert(key, b"v1", hash).expect("fits");
    let image = table.record_bytes(cold, table.record(cold).encoded_len).to_vec();
    let tail = table.space().tail();
    let space = table.space_mut();
    space.advance_ro_boundary(tail);
    space.advance_flushed(tail);
    space.advance_head(tail);
    assert_eq!(table.space().resolve(cold), AddrClass::Cold);
    assert_eq!(table.shadow_probe(key, hash), ShadowProbe::One(cold));
    let record_len = TieredTable::RECORD_HEADER_LEN + key.len() + 2;
    table.shadow_admit(hash, cold, record_len).expect("admitted");
    let winner = table.insert(key, b"v2", hash).expect("fits");
    table.register_shadow(hash, cold, winner);
    assert_eq!(table.shadow_unverified(), 1);
    (hash, cold, image)
}

fn shadow_read_errors(shared: &Shared<NoopObserver, StdSegmentFs>) -> u64 {
    let store = shared.store.borrow();
    store.tiered_store(TIERED).expect("materialized").shadow_counters().read_errors
}

/// `argv` on a connection bound to the tiered namespace, dispatched and
/// settled as the connection's pump does: the connection's next reply.
#[allow(clippy::disallowed_types, reason = "test-only: the dispatcher's reply queue (ADR-0163 D2)")]
fn tiered_command(cells: &Cells, argv: &[&[u8]]) -> OwnedReply {
    let shared = cells.origin();
    let meta = lookup(argv[0]).expect("a registered command");
    cells.drive(
        async {
            let (mut pending, mut inflight) = (std::collections::VecDeque::new(), 0);
            let proto = Protocol::Resp2;
            let live = dispatch_ns(
                shared,
                KEY,
                ORIGIN,
                meta,
                argv,
                proto,
                0,
                0,
                TIERED,
                &mut pending,
                &mut inflight,
            )
            .await;
            assert!(live, "the connection stays");
            let front = pending.pop_front().expect("one reply slot");
            assert!(pending.is_empty(), "one command, one reply slot");
            settle_reply(shared, KEY, front, &mut inflight).await
        },
        no_answer,
    )
}

/// A tiered peer whose `DBSIZE` drain errors: its twin read fails at the
/// fault point that stands in for a device error, and the peer's ns-apply
/// pump sends the drain's error reply tagged from its outcome (ADR-0190
/// D3). The client reads that error, `Error` by its outcome, where a
/// reply tagged a success would earn the fold's fail-closed internal
/// error. The connection's replies before and after it are exact counts.
#[test]
fn reply_outcome_tiered_dbsize_drain_error_is_the_reply() {
    let cells = Cells::new();
    tiered_namespace(&cells);
    let peer = &cells.planes[1].shared;
    let before = tiered_command(&cells, &[b"DBSIZE"]);
    let (hash, cold, image) = open_unread_twin(peer, &cells.key_on(1));
    inf_foundation::fault::arm(SHADOW_TWIN_READ_FAIL, FaultSpec::Always);
    let own = ns_dbsize_local(peer, ExecOrigin::Fabric(CellId(0)), TIERED, Protocol::Resp2);
    let expected = cells.drive(own, no_answer).expect_err("the peer's own drain fails its read");
    let peer_errors = shadow_read_errors(peer);
    let failed = tiered_command(&cells, &[b"DBSIZE"]);
    inf_foundation::fault::disarm(SHADOW_TWIN_READ_FAIL);
    assert_eq!(shadow_read_errors(peer) - peer_errors, 1, "VACUOUS: the peer's drain read no twin");
    assert_eq!(shadow_read_errors(cells.origin()), 0, "the origin's own leg read no twin");
    assert!(expected.is_error(), "the drain's reply is an error");
    engaged(expected.bytes());
    assert_eq!(
        String::from_utf8_lossy(failed.bytes()),
        String::from_utf8_lossy(expected.bytes()),
        "the client reads the peer's drain error"
    );
    assert_eq!(failed.outcome(), ReplyOutcome::Error);
    // Healed: the twin's image verifies the ticket, so the next drain
    // reads nothing and counts the key once.
    let verdict = {
        let mut store = peer.store.borrow_mut();
        let table = store.tiered_store_mut(TIERED).expect("materialized");
        table.verify_shadow(hash, cold, &image)
    };
    assert_eq!(verdict, ShadowVerdict::SameKey);
    let after = tiered_command(&cells, &[b"DBSIZE"]);
    assert_eq!(before.outcome(), ReplyOutcome::Success);
    assert_eq!(after.outcome(), ReplyOutcome::Success);
    let mut out = ReplyStream::new(Vec::new());
    for reply in [&before, &failed, &after] {
        out.emit(reply);
    }
    assert_eq!(out.as_bytes(), [b":0\r\n".as_slice(), expected.bytes(), b":1\r\n"].concat());
}

/// A tiered peer's `SCAN` refusal through its ns-apply pump's tiered arm:
/// the cursor addresses cell 1, so the one leg runs on cell 1's pump,
/// whose tiered `SCAN` refuses `MATCH` (the string family only), and the
/// pump tags that reply from its outcome (ADR-0190 D3). The client reads
/// the peer's own refusal, `Error` by its outcome, where a reply tagged a
/// success would read as a scan leg without its head and earn the
/// malformed-reply error. A client reaches it when its cursor has moved to
/// the next cell and the call carries an option.
#[test]
fn reply_outcome_tiered_scan_refusal_is_the_reply() {
    let cells = Cells::new();
    tiered_namespace(&cells);
    let peer = &cells.planes[1].shared;
    let leg: &[&[u8]] = &[b"SCAN", b"0", b"MATCH", b"x"];
    let meta = lookup(leg[0]).expect("a registered command");
    let class = peer.store.borrow().ns_fsync_class(TIERED);
    let origin = ExecOrigin::Fabric(CellId(0));
    let own = tiered::dispatch_tiered(peer, origin, TIERED, meta, leg, Protocol::Resp2, class);
    let tiered::TieredReply::Done(expected) = cells.drive(own, no_answer) else {
        panic!("a read is never gated on a durable write");
    };
    assert!(expected.is_error(), "the peer's tiered SCAN refuses MATCH");
    engaged(expected.bytes());
    let cursor = (1u64 << SCAN_CELL_SHIFT).to_string();
    let reply = tiered_command(&cells, &[b"SCAN", cursor.as_bytes(), b"MATCH", b"x"]);
    assert_eq!(cells.pumped[1].get(), 1, "VACUOUS: no leg reached the peer's ns-apply pump");
    assert_eq!(cells.pumped[0].get(), 0, "the origin's pumps took nothing");
    assert_eq!(
        String::from_utf8_lossy(reply.bytes()),
        String::from_utf8_lossy(expected.bytes()),
        "the client reads the peer's refusal"
    );
    assert_eq!(reply.outcome(), ReplyOutcome::Error);
}

/// An invalid `CONFIG SET`: the local leg's error is the reply and nothing
/// fans.
#[test]
fn reply_outcome_config_set_invalid_is_the_reply() {
    let cells = Cells::new();
    let argv: &[&[u8]] = &[b"CONFIG", b"SET", b"maxmemory", b"bogus"];
    let expected = cells.local(0, argv);
    engaged(&expected);
    let shared = cells.origin();
    let mut fanned = 0;
    let reply = cells.drive(
        async {
            let mut inflight = 0;
            let pending = dispatch_scatter(
                shared,
                CommandId::Config,
                ORIGIN,
                Protocol::Resp2,
                0,
                0,
                argv,
                &mut inflight,
            )
            .await;
            settle_reply(shared, KEY, pending, &mut inflight).await.into_bytes()
        },
        |args| {
            fanned += usize::from(args[0] == b"CONFIG");
            None
        },
    );
    assert_eq!(reply, expected);
    assert_eq!(fanned, 0, "a refused local leg fans nothing");
}

/// `COPY` whose put leg the destination refuses: the client reads that
/// refusal, not a malformed-reply error.
#[test]
fn reply_outcome_copy_refused_put_is_the_reply() {
    let cells = Cells::new();
    let source = cells.key_on(0);
    let target = cells.key_on(1);
    assert_eq!(cells.local(0, &[b"SET", &source, b"v"]), b"+OK\r\n");
    let refusal = refuse_writes(&cells, 1);
    let shared = cells.origin();
    let argv: &[&[u8]] = &[b"COPY", &source, &target];
    let reply = cells.drive(
        async {
            program_move(shared, ORIGIN, Protocol::Resp2, 0, 0, CommandId::Copy, argv)
                .await
                .into_bytes()
        },
        no_answer,
    );
    assert_eq!(reply, refusal);
}

/// A `PUBSUB CHANNELS` leg that succeeded without an array is a malformed
/// peer reply, never the client's whole answer.
#[test]
fn reply_outcome_channels_success_without_array_is_malformed() {
    let cells = Cells::new();
    let reply = cells.drive(channels(&cells), |args| {
        (args[0] == b"INF.PUBSUB").then_some(Outcome::Bytes(b"+OK\r\n"))
    });
    assert_eq!(reply, written_error("ERR cross-cell program reply malformed"));
}

/// Control: a peer's error leg is passed through untouched.
#[test]
fn reply_outcome_channels_error_leg_is_the_reply() {
    let cells = Cells::new();
    let reply = cells.drive(channels(&cells), |args| {
        (args[0] == b"INF.PUBSUB").then_some(Outcome::ErrorReply(b"-ERR x\r\n"))
    });
    assert_eq!(reply, b"-ERR x\r\n");
}

async fn channels(cells: &Cells) -> Vec<u8> {
    program_pubsub(cells.origin(), Protocol::Resp2, &[b"PUBSUB", b"CHANNELS"]).await.into_bytes()
}

/// A `KEYS` peer leg that succeeded without an array header is a malformed
/// peer reply, never the client's whole answer.
#[test]
fn reply_outcome_keys_success_leg_without_header_is_malformed() {
    let cells = Cells::new();
    let reply = cells
        .drive(keys(&cells), |args| (args[0] == b"KEYS").then_some(Outcome::Bytes(b"+OK\r\n")));
    assert_eq!(reply, written_error("ERR cross-cell program reply malformed"));
}

/// Control: a `KEYS` peer leg's error is the reply, untouched, whatever its
/// first byte.
#[test]
fn reply_outcome_keys_error_leg_is_the_reply() {
    let cells = Cells::new();
    let reply =
        cells.drive(keys(&cells), |args| (args[0] == b"KEYS").then_some(Outcome::ErrorReply(BLOB)));
    assert_eq!(reply, BLOB);
}

/// A `SCAN` leg on the peer's cell that succeeded without its head is a
/// malformed peer reply, never the client's whole answer.
#[test]
fn reply_outcome_scan_success_leg_without_head_is_malformed() {
    let cells = Cells::new();
    let reply = cells.drive(scan_on_peer(&cells), |args| {
        (args[0] == b"SCAN").then_some(Outcome::Bytes(b"+OK\r\n"))
    });
    assert_eq!(reply, written_error("ERR cross-cell program reply malformed"));
}

/// Control: a `SCAN` leg's error is the reply, untouched, whatever its
/// first byte.
#[test]
fn reply_outcome_scan_error_leg_is_the_reply() {
    let cells = Cells::new();
    let reply = cells.drive(scan_on_peer(&cells), |args| {
        (args[0] == b"SCAN").then_some(Outcome::ErrorReply(BLOB))
    });
    assert_eq!(reply, BLOB);
}

/// A leg reply as the decode builds it, its outcome from the wire.
fn leg(bytes: &[u8], outcome: ReplyOutcome) -> OwnedReply {
    OwnedReply::received(bytes.to_vec(), outcome).expect("a non-empty leg")
}

/// An array leg's three answers, decided by its reader: an `Error` leg is
/// the reply untouched, a `Success` leg without a header the malformed
/// refusal, a well-formed one its count and body. No producer answers a
/// `KEYS` local leg a headerless success, so this row is that leg's.
#[test]
fn reply_outcome_array_leg_reader_answers_each_outcome() {
    let cells = Cells::new();
    let shared = cells.origin();
    let read = |reply| read_array_leg(shared, Protocol::Resp2, reply);
    let passed = read(leg(BLOB, ReplyOutcome::Error)).expect_err("an error leg");
    assert_eq!((passed.bytes(), passed.is_error()), (BLOB, true));
    let refused = read(leg(b"+OK\r\n", ReplyOutcome::Success)).expect_err("a malformed success");
    assert_eq!(refused.bytes(), written_error(PROGRAM_REPLY_MALFORMED));
    assert!(refused.is_error());
    let array = read(leg(b"*1\r\n$1\r\nk\r\n", ReplyOutcome::Success)).expect("a well-formed leg");
    assert_eq!((array.count(), array.body()), (1, &b"$1\r\nk\r\n"[..]));
}

/// A scan leg's three answers, decided by its reader.
#[test]
fn reply_outcome_scan_leg_reader_answers_each_outcome() {
    let cells = Cells::new();
    let shared = cells.origin();
    let read = |reply| read_scan_leg(shared, Protocol::Resp2, reply);
    let passed = read(leg(BLOB, ReplyOutcome::Error)).expect_err("an error leg");
    assert_eq!((passed.bytes(), passed.is_error()), (BLOB, true));
    let refused = read(leg(b"*1\r\n", ReplyOutcome::Success)).expect_err("a malformed success");
    assert_eq!(refused.bytes(), written_error(PROGRAM_REPLY_MALFORMED));
    assert!(refused.is_error());
    let head = read(leg(b"*2\r\n$1\r\n7\r\n*0\r\n", ReplyOutcome::Success)).expect("a well-formed");
    assert_eq!((head.cursor(), head.rest()), (7, &b"*0\r\n"[..]));
}

async fn keys(cells: &Cells) -> Vec<u8> {
    let argv: &[&[u8]] = &[b"KEYS", b"*"];
    program_keys(cells.origin(), ORIGIN, Protocol::Resp2, 0, 0, ScatterScope::Db, argv)
        .await
        .into_bytes()
}

/// `SCAN` at the first cursor of cell 1, so the one leg crosses the fabric.
async fn scan_on_peer(cells: &Cells) -> Vec<u8> {
    let cursor = (1u64 << SCAN_CELL_SHIFT).to_string();
    let argv: &[&[u8]] = &[b"SCAN", cursor.as_bytes()];
    program_scan(cells.origin(), ORIGIN, Protocol::Resp2, 0, 0, ScatterScope::Db, argv)
        .await
        .into_bytes()
}

/// A peer's error in RESP3's second encoding (`!`), carried by the wire's
/// error tag: the fan's reply is that error, whatever its first byte. The
/// plant-free arm of the blob-error row, kept on the plain build.
#[test]
fn reply_outcome_blob_error_leg_is_the_reply() {
    let cells = Cells::new();
    let remote = cells.key_on(1);
    let shared = cells.origin();
    let argv: &[&[u8]] = &[b"MSET", &remote, b"v"];
    let reply = cells.drive(
        async {
            let mut inflight = 0;
            let pending =
                program_mset(shared, ORIGIN, Protocol::Resp2, 0, 0, argv, &mut inflight).await;
            settle_reply(shared, KEY, pending, &mut inflight).await.into_bytes()
        },
        |args| (args[0] == b"SET").then_some(Outcome::ErrorReply(BLOB)),
    );
    assert_eq!(reply, BLOB);
}

/// A `-` reply the wire tags a success is a success leg: the tag decides,
/// the bytes are never read.
#[test]
fn reply_outcome_dash_reply_tagged_success_is_a_success_leg() {
    let cells = Cells::new();
    let remote = cells.key_on(1);
    let shared = cells.origin();
    let argv: &[&[u8]] = &[b"MSET", &remote, b"v"];
    let reply = cells.drive(
        async {
            let mut inflight = 0;
            let pending =
                program_mset(shared, ORIGIN, Protocol::Resp2, 0, 0, argv, &mut inflight).await;
            settle_reply(shared, KEY, pending, &mut inflight).await.into_bytes()
        },
        |args| (args[0] == b"SET").then_some(Outcome::Bytes(b"-ERR x\r\n")),
    );
    assert_eq!(reply, b"+OK\r\n");
}

/// A leg that answers zero bytes is refused once, at the decode, and the
/// client reads the one refusal text.
#[test]
fn reply_outcome_empty_leg_is_refused() {
    let cells = Cells::new();
    let remote = cells.key_on(1);
    let shared = cells.origin();
    let argv: &[&[u8]] = &[b"GET", &remote];
    let reply = cells.drive(
        async {
            let mut inflight = 1;
            let waiter =
                send_apply(shared, CellId(1), ApplyOrigin::Client, Protocol::Resp2, 0, argv)
                    .await
                    .expect("the leg is sent");
            let remote = PendingReply::Remote { waiter, proto: Protocol::Resp2 };
            settle_reply(shared, KEY, remote, &mut inflight).await.into_bytes()
        },
        |args| (args[0] == b"GET").then_some(Outcome::Bytes(b"")),
    );
    assert_eq!(reply, written_error(CROSS_CELL_EXECUTION_FAILED));
}

/// `Op::Read` stages the bare value, no frame: a hit on an empty value is
/// a zero-byte `Success` reply, which the origin refuses as every empty
/// leg is (`reply_outcome_empty_leg_is_refused`). The op has no production
/// sender; this pins the consequence until one gives the value a frame.
#[test]
fn reply_outcome_read_of_an_empty_value_stages_zero_bytes() {
    let cells = Cells::new();
    let key = cells.key_on(1);
    assert_eq!(cells.local(1, &[b"SET", &key, b""]), b"+OK\r\n");
    let shared = &cells.planes[1].shared;
    let (mut scratch, mut staged, mut pubs, mut gated, mut orphans) =
        (ReplyStream::new(Vec::new()), Vec::new(), Vec::new(), Vec::new(), 0u64);
    let read = Op::Read { token: FabricToken(1), slot: KeySlot::of_key(&key), key: &key };
    let now = shared.now.get();
    handle_fabric_op(
        shared,
        now,
        CellId(0),
        read,
        &mut scratch,
        &mut staged,
        &mut pubs,
        &mut gated,
        &mut orphans,
    );
    let [(_, _, reply)] = staged.as_slice() else { panic!("one staged reply: {}", staged.len()) };
    assert!(matches!(staged_outcome(scratch.as_bytes(), reply), Outcome::Bytes(b"")));
}

/// `MSETNX`'s `SET` fan: a leg's error reply is the client's reply, read by
/// its outcome, whatever its first byte (ADR-0190 D2).
#[test]
fn reply_outcome_msetnx_set_leg_error_is_the_reply() {
    let cells = Cells::new();
    let remote = cells.key_on(1);
    let shared = cells.origin();
    let argv: &[&[u8]] = &[b"MSETNX", &remote, b"v"];
    let reply = cells.drive(
        async { program_msetnx(shared, ORIGIN, Protocol::Resp2, 0, 0, argv).await.into_bytes() },
        |args| (args[0] == b"SET").then_some(Outcome::ErrorReply(BLOB)),
    );
    assert_eq!(reply, BLOB);
}

/// A move's read leg (`INF.PEEK`) that errs: the client reads that error,
/// not a malformed-reply error.
#[test]
fn reply_outcome_copy_read_leg_error_is_the_reply() {
    let cells = Cells::new();
    let (source, target) = (cells.key_on(1), cells.key_on(0));
    let shared = cells.origin();
    let argv: &[&[u8]] = &[b"COPY", &source, &target];
    let reply = cells.drive(
        async {
            program_move(shared, ORIGIN, Protocol::Resp2, 0, 0, CommandId::Copy, argv)
                .await
                .into_bytes()
        },
        |args| (args[0] == b"INF.PEEK").then_some(Outcome::ErrorReply(BLOB)),
    );
    assert_eq!(reply, BLOB);
}

/// A move's conditional remove (`INF.TAKE`) that errs after the put: the
/// client reads that error, not a malformed-reply error.
#[test]
fn reply_outcome_rename_take_leg_error_is_the_reply() {
    let cells = Cells::new();
    let (source, target) = (cells.key_on(1), cells.key_on(0));
    assert_eq!(cells.local(1, &[b"SET", &source, b"v"]), b"+OK\r\n");
    let shared = cells.origin();
    let argv: &[&[u8]] = &[b"RENAME", &source, &target];
    let reply = cells.drive(
        async {
            program_move(shared, ORIGIN, Protocol::Resp2, 0, 0, CommandId::Rename, argv)
                .await
                .into_bytes()
        },
        |args| (args[0] == b"INF.TAKE").then_some(Outcome::ErrorReply(BLOB)),
    );
    assert_eq!(reply, BLOB);
}

/// `JSON.MGET` over one local and one remote key, the remote leg answered
/// by `answer`; the client's reply.
#[cfg(feature = "doc")]
fn json_mget(
    cells: &Cells,
    answer: impl FnMut(&[&[u8]]) -> Option<Outcome<'static>>,
) -> (Vec<u8>, Vec<u8>) {
    let (local, remote) = (cells.key_on(0), cells.key_on(1));
    let element = cells.local(0, &[b"JSON.MGET", &local, b"$.a"]);
    let element = element.strip_prefix(b"*1\r\n").expect("a one-element sub-reply").to_vec();
    let shared = cells.origin();
    let argv: &[&[u8]] = &[b"JSON.MGET", &local, &remote, b"$.a"];
    let reply = cells.drive(
        async {
            let mut inflight = 0;
            let pending =
                gather_json_mget(shared, ORIGIN, Protocol::Resp2, 0, 0, argv, &mut inflight).await;
            settle_reply(shared, KEY, pending, &mut inflight).await.into_bytes()
        },
        answer,
    );
    (reply, element)
}

/// The `JSON.MGET` gather pastes an `Error` element whole, whatever its
/// first byte: no shape is read from an error (ADR-0190 D2).
#[cfg(feature = "doc")]
#[test]
fn reply_outcome_json_mget_error_element_is_pasted_whole() {
    let cells = Cells::new();
    let (reply, element) =
        json_mget(&cells, |args| (args[0] == b"JSON.MGET").then_some(Outcome::ErrorReply(BLOB)));
    assert_eq!(reply, [b"*2\r\n".as_slice(), &element, BLOB].concat());
}

/// Control: a `Success` element is stripped to the one value it carries.
#[cfg(feature = "doc")]
#[test]
fn reply_outcome_json_mget_success_element_is_stripped() {
    let cells = Cells::new();
    let (reply, element) = json_mget(&cells, |args| {
        (args[0] == b"JSON.MGET").then_some(Outcome::Bytes(b"*1\r\n$5\r\n[1,2]\r\n"))
    });
    assert_eq!(reply, [b"*2\r\n".as_slice(), &element, b"$5\r\n[1,2]\r\n"].concat());
}
