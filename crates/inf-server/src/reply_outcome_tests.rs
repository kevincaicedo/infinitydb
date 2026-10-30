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
use inf_store::{Keyspace, StoreConfig};

use super::dispatch::{dispatch_scatter, program_mset, settle_reply};
use super::publish::program_pubsub;
use super::*;

/// The client-side key every settled reply is written for (no connection
/// is live: a publish stash lookup finds none).
const KEY: ConnKey = ConnKey { slot: 0, generation: 0 };
const ORIGIN: ExecOrigin = ExecOrigin::Conn(0, 0);

struct Cells {
    planes: Vec<ServerPlane<NoopObserver>>,
}

impl Cells {
    fn new() -> Self {
        let planes = Mesh::new(2, MeshConfig::default())
            .into_iter()
            .enumerate()
            .map(|(cell, fabric)| {
                let (plane, _serve_mark) = ServerPlane::new(
                    CellId(cell as u16),
                    2,
                    -1,
                    Keyspace::new(StoreConfig::default()),
                    fabric,
                    Rc::new(NodeInfo::try_default().expect("fixture cache allocation")),
                    NoopObserver,
                    false,
                );
                plane
            })
            .collect();
        Cells { planes }
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
            for plane in &self.planes {
                drain(&plane.shared, &mut answer);
            }
        }
        panic!("the program exceeded its bounded legs");
    }
}

/// One FABRIC-IN pass of `shared`, then its staged replies shipped.
fn drain(
    shared: &Shared<NoopObserver, StdSegmentFs>,
    answer: &mut impl FnMut(&[&[u8]]) -> Option<Outcome<'static>>,
) {
    let (mut scratch, mut staged, mut pubs, mut gated, mut orphans) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new(), 0u64);
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
        fabric.reply(to, token, &staged_outcome(&scratch, &reply));
    }
    for (to, token, outcome) in answered {
        fabric.reply(to, token, &outcome);
    }
    fabric.flush();
}

fn no_answer(_: &[&[u8]]) -> Option<Outcome<'static>> {
    None
}

/// An error as this build's writer encodes it, so a row reads the same
/// under the blob-error plant as on the plain build.
fn written_error(text: &str) -> Vec<u8> {
    let mut bytes = Vec::new();
    RespWriter::new(&mut ReplyBuf::over(&mut bytes), Protocol::Resp2).error(text);
    bytes
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
        |args| (args[0] == b"SET").then_some(Outcome::ErrorReply(b"!5\r\nERR x\r\n")),
    );
    assert_eq!(reply, b"!5\r\nERR x\r\n");
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
        (Vec::new(), Vec::new(), Vec::new(), Vec::new(), 0u64);
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
    assert!(matches!(staged_outcome(&scratch, reply), Outcome::Bytes(b"")));
}
