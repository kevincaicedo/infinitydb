//! H3: poll the real move future and schedule actual Apply legs explicitly.
//! The fixture controls interleavings without clocks, threads or allocator luck.

use std::future::Future;
use std::task::{Context, Poll, Waker};

use inf_fabric::{Mesh, MeshConfig};
use inf_store::{Keyspace, StoreConfig};

use super::*;

/// Snapshot framing must be complete before any destination write.
#[test]
fn malformed_snapshot_never_reaches_destination() {
    for raw in [
        b"*2\r\n$7\r\npayrollXX:60000\r\n".as_slice(),
        b"*2\r\n$7\r\npayroll\r\n:60000\r\n+OK\r\n",
        b"*-1\r\n+OK\r\n",
    ] {
        for command in [CommandId::Rename, CommandId::Renamenx, CommandId::Copy] {
            let rig = Rig::new(1);
            rig.seed();
            let mut snapshots = 0;
            let reply = rig.run(command, |args, _| {
                assert_ne!(args[0], b"SET", "H3 follow-up: malformed snapshot reached destination");
                (args[0] == b"INF.PEEK").then(|| {
                    snapshots += 1;
                    raw.to_vec()
                })
            });
            assert_eq!(snapshots, 1, "the snapshot must cross the controlled fabric leg");
            assert_eq!(reply, b"-ERR cross-cell program reply malformed\r\n");
            assert_eq!(rig.source(&[b"GET", &rig.source]), b"$7\r\npayroll\r\n");
            assert_eq!(rig.local(0, &[b"EXISTS", &rig.target]), b":0\r\n");
        }
    }
}

/// Neither missing nor present snapshots may hide trailing frames.
#[test]
fn snapshot_parser_rejects_bad_delimiters_and_trailing_bytes() {
    for raw in [
        b"*2\r\n$1\r\nvXX:-1\r\n".as_slice(),
        b"*2\r\n$1\r\nv\r\n:-1\r\n+OK\r\n",
        b"*-1\r\n+OK\r\n",
    ] {
        assert!(parse_take_reply(raw).is_none(), "H3 follow-up: malformed snapshot accepted");
    }
}

/// Length arithmetic is total even at the platform width boundary.
#[test]
fn snapshot_length_overflow_is_rejected() {
    for len in [usize::MAX, usize::MAX - 1, usize::MAX - 20] {
        let raw = format!("*2\r\n${len}\r\nx\r\n:-1\r\n");
        assert!(parse_take_reply(raw.as_bytes()).is_none());
    }
}

/// A live record whose absolute deadline is ≤ 0 exists only under an
/// injected anchor with `internal > unix` (no boot produces one). The
/// destination put is bound by its own gate — `INF.PUT`'s deadline rule
/// for the renames (ADR-0110 third amendment), SET's Redis gate for COPY
/// (M1, batch 16): the move refuses it and the source is preserved
/// (ADR-0110 second amendment — the first amendment's acceptance stood on
/// the M1 defect).
#[test]
fn zero_unix_deadline_refuses_the_move_and_preserves_the_source() {
    for owner in [0, 1] {
        for command in [CommandId::Rename, CommandId::Renamenx, CommandId::Copy] {
            let rig = Rig::new(owner);
            for plane in &rig.planes {
                plane.shared.node.wall_anchor.set((60_000, 0));
            }
            rig.seed();
            assert_eq!(rig.source(&[b"PEXPIRETIME", &rig.source]), b":0\r\n");
            let reply = rig.run(command, |_, _| None);
            let expected: &[u8] = if command == CommandId::Copy {
                b"-ERR invalid expire time in 'set' command\r\n"
            } else {
                b"-ERR invalid move snapshot deadline\r\n"
            };
            assert_eq!(
                reply, expected,
                "batch 16/17: a non-positive absolute deadline never reaches the store"
            );
            assert_eq!(rig.source(&[b"GET", &rig.source]), b"$7\r\npayroll\r\n");
            assert_eq!(rig.source(&[b"PEXPIRETIME", &rig.source]), b":0\r\n");
            assert_eq!(rig.local(1 - owner, &[b"GET", &rig.target]), b"$-1\r\n");
        }
    }
}

/// With the default anchor, `PXAT 0` is refused at the SET (M1): the key
/// never exists, and the move answers missing.
#[test]
fn default_anchor_zero_deadline_is_missing() {
    let rig = Rig::new(0);
    assert_eq!(
        rig.source(&[b"SET", &rig.source, b"v", b"PXAT", b"0"]),
        b"-ERR invalid expire time in 'set' command\r\n"
    );
    assert_eq!(rig.run(CommandId::Rename, |_, _| None), b"-ERR no such key\r\n");
}

/// Internal reads do not inflate client hit/miss counters.
#[test]
fn rename_does_not_count_internal_reads() {
    for owner in [0, 1] {
        for command in [CommandId::Rename, CommandId::Renamenx] {
            let rig = Rig::new(owner);
            rig.seed();
            let stats = || rig.planes[owner].shared.store.borrow_mut().db_mut(0).stats();
            let before = stats();
            let _ = rig.run(command, |_, _| None);
            let after = stats();
            assert_eq!(after.keyspace_hits, before.keyspace_hits, "H3 follow-up: internal hits");
            assert_eq!(after.keyspace_misses, before.keyspace_misses);
        }
    }
}

/// RENAMENX cannot identify its own leftover copy on retry after BUSY.
#[test]
fn renamenx_retry_after_busy_is_a_noop() {
    let rig = Rig::new(0);
    rig.seed();
    let reply = rig.run(CommandId::Renamenx, |args, rig| {
        if is_put(args) {
            assert_eq!(rig.source(&[b"SET", &rig.source, b"newer"]), b"+OK\r\n");
        }
        None
    });
    assert_eq!(
        reply,
        b"-BUSY source changed during cross-cell move; destination may contain a copy\r\n"
    );
    assert_eq!(rig.run(CommandId::Renamenx, |_, _| None), b":0\r\n");
    assert_eq!(rig.source(&[b"GET", &rig.source]), b"$5\r\nnewer\r\n");
    assert_eq!(rig.local(1, &[b"GET", &rig.target]), b"$7\r\npayroll\r\n");
}

/// ADR-0115: on the plane, a client-shaped execution (`program = false`,
/// the mirror/fast-path shape) of an internal row is unknown; the
/// program's own leg (`run_local`) runs it.
#[test]
fn a_client_typed_internal_command_is_unknown_on_the_plane() {
    let rig = Rig::new(0);
    let shared = &rig.planes[0].shared;
    let argv: &[&[u8]] = &[b"INF.PUT", &rig.source, b"v", b"-1"];
    let mut reply = Vec::new();
    shared.execute_owned_into(
        ExecOrigin::Conn(0, 0),
        argv,
        Protocol::Resp2,
        1,
        0,
        None,
        false,
        &mut reply,
    );
    assert!(reply.starts_with(b"-ERR unknown command 'INF.PUT'"), "{reply:?}");
    assert_eq!(rig.source(&[b"GET", &rig.source]), b"$-1\r\n", "nothing landed");
    assert_eq!(rig.local(0, argv), b"+OK\r\n", "the program leg runs");
    assert_eq!(rig.source(&[b"GET", &rig.source]), b"$1\r\nv\r\n");
}

/// ADR-0115 D3/D5: the owner executes an internal row on a fabric frame
/// only when the frame carries the program mark — an unmarked frame
/// (a forwarded client command) answers unknown, the marked twin lands.
#[test]
fn an_unmarked_apply_carrying_an_internal_command_is_unknown() {
    let rig = Rig::new(0);
    let owner = 1;
    let key = &rig.target; // owned by cell 1
    let shared = &rig.planes[owner].shared;
    let mut replies = Vec::new();
    for (seq, program) in [(1u64, false), (2, true)] {
        let args = ApplyArgs::new(&[b"INF.PUT".as_slice(), key, b"v", b"-1"]).unwrap();
        let op = Op::Apply {
            token: FabricToken::new(CellId(0), seq),
            slot: SlotRouter::slot_of(key),
            cmd: 2,
            args,
            program,
        };
        let (mut scratch, mut staged, mut pubs, mut gated, mut orphans) =
            (Vec::new(), Vec::new(), Vec::new(), Vec::new(), 0u64);
        handle_fabric_op(
            shared,
            shared.now.get(),
            CellId(0),
            op,
            &mut scratch,
            &mut staged,
            &mut pubs,
            &mut gated,
            &mut orphans,
        );
        let (_, _, reply) = staged.pop().expect("one staged reply");
        let StagedReply::Bytes(start, end) = reply else { panic!("expected raw reply bytes") };
        let bytes = scratch[start..end].to_vec();
        replies.push(bytes);
    }
    assert!(replies[0].starts_with(b"-ERR unknown command 'INF.PUT'"), "{:?}", replies[0]);
    assert_eq!(replies[1], b"+OK\r\n");
    assert_eq!(rig.local(owner, &[b"GET", key]), b"$1\r\nv\r\n", "only the marked frame landed");
}

/// L12 style row (review of 2026-08-30): `handle_apply` indexed `argv[0]`
/// bare while the codec admits a zero-argument `Apply`. An empty apply is
/// an in-process malformation — answered with the typed refusal (the
/// origin's credit comes back), never a cell panic.
#[test]
fn an_apply_without_arguments_is_refused_not_a_panic() {
    let rig = Rig::new(0);
    let shared = &rig.planes[1].shared;
    for program in [false, true] {
        let op = Op::Apply {
            token: FabricToken::new(CellId(0), 1),
            slot: SlotRouter::slot_of(&rig.target),
            cmd: 2,
            args: ApplyArgs::EMPTY,
            program,
        };
        let (mut scratch, mut staged, mut pubs, mut gated, mut orphans) =
            (Vec::new(), Vec::new(), Vec::new(), Vec::new(), 0u64);
        handle_fabric_op(
            shared,
            shared.now.get(),
            CellId(0),
            op,
            &mut scratch,
            &mut staged,
            &mut pubs,
            &mut gated,
            &mut orphans,
        );
        let (_, _, reply) = staged.pop().expect("one staged reply");
        assert!(matches!(reply, StagedReply::Refused), "program={program}");
    }
}

/// The destination leg: `INF.PUT` for the renames, `SET` for COPY
/// (ADR-0110 third amendment).
fn is_put(args: &[&[u8]]) -> bool {
    matches!(args[0], b"SET" | b"INF.PUT")
}

struct Rig {
    planes: Vec<ServerPlane<NoopObserver>>,
    source: Vec<u8>,
    target: Vec<u8>,
    source_owner: usize,
}

impl Rig {
    fn new(source_owner: usize) -> Self {
        let planes: Vec<_> = Mesh::new(2, MeshConfig::default())
            .into_iter()
            .enumerate()
            .map(|(cell, fabric)| {
                ServerPlane::new(
                    CellId(cell as u16),
                    2,
                    -1,
                    Keyspace::new(StoreConfig::default()),
                    fabric,
                    Rc::new(NodeInfo::default()),
                    NoopObserver,
                    false,
                )
            })
            .collect();
        let key = |owner| {
            (0..1000)
                .map(|n| format!("move:{n}").into_bytes())
                .find(|k| planes[0].shared.router.cell_of(SlotRouter::slot_of(k)) == CellId(owner))
                .expect("both owners reachable")
        };
        let source = key(source_owner as u16);
        let target = key(1 - source_owner as u16);
        Self { planes, source, target, source_owner }
    }

    fn local(&self, owner: usize, argv: &[&[u8]]) -> Vec<u8> {
        run_local(&self.planes[owner].shared, ExecOrigin::Conn(0, 0), Protocol::Resp2, 0, 0, argv)
    }

    fn source(&self, argv: &[&[u8]]) -> Vec<u8> {
        self.local(self.source_owner, argv)
    }

    fn seed(&self) {
        assert_eq!(self.source(&[b"SET", &self.source, b"payroll", b"PX", b"60000"]), b"+OK\r\n");
    }

    fn run(
        &self,
        command: CommandId,
        mut before: impl FnMut(&[&[u8]], &Self) -> Option<Vec<u8>>,
    ) -> Vec<u8> {
        let verb: &[u8] = match command {
            CommandId::Rename => b"RENAME",
            CommandId::Renamenx => b"RENAMENX",
            CommandId::Copy => b"COPY",
            _ => unreachable!("the move fixture accepts only move commands"),
        };
        let argv: &[&[u8]] = &[verb, &self.source, &self.target];
        let mut future = Box::pin(program_move(
            &self.planes[0].shared,
            ExecOrigin::Conn(0, 0),
            Protocol::Resp2,
            0,
            0,
            command,
            argv,
        ));
        let mut cx = Context::from_waker(Waker::noop());
        for _ in 0..16 {
            if let Poll::Ready(reply) = future.as_mut().poll(&mut cx) {
                return reply;
            }
            for plane in &self.planes {
                plane.shared.fabric.borrow_mut().flush();
            }
            for (owner, plane) in self.planes.iter().enumerate() {
                let mut requests = Vec::new();
                plane.shared.fabric.borrow_mut().drain(16, |from, op| {
                    if let Op::Apply { token, args, .. } = op {
                        requests.push((
                            from,
                            token,
                            args.as_slice().iter().map(|a| a.to_vec()).collect::<Vec<_>>(),
                        ));
                    } else {
                        panic!("move emits only Apply");
                    }
                });
                for (from, token, owned) in requests {
                    let args: Vec<&[u8]> = owned.iter().map(Vec::as_slice).collect();
                    let reply = before(&args, self).unwrap_or_else(|| self.local(owner, &args));
                    let outcome = if args[0] == b"EXISTS" {
                        // The real plane emits typed counts for EXISTS (count_on).
                        let count = std::str::from_utf8(&reply[1..reply.len() - 2])
                            .expect("integer reply")
                            .parse()
                            .expect("count");
                        OwnedOutcome::Int(count)
                    } else {
                        OwnedOutcome::Bytes(reply)
                    };
                    assert!(
                        self.planes[usize::from(from.0)].shared.gate.complete(token.0, outcome)
                    );
                }
            }
        }
        panic!("move exceeded its bounded legs");
    }
}

/// Every refused destination leaves the complete source snapshot unchanged.
#[test]
fn destination_refusal_preserves_source() {
    for command in [CommandId::Rename, CommandId::Renamenx, CommandId::Copy] {
        for error in [
            b"-OOM command not allowed when used memory > 'maxmemory'.\r\n".as_slice(),
            b"-ERR key or value exceeds InfinityDB M0 record bounds\r\n",
            b"-ERR cross-cell execution failed\r\n",
        ] {
            let rig = Rig::new(0);
            rig.seed();
            let before = rig.source(&[b"INF.PEEK", &rig.source]);
            let reply = rig.run(command, |args, _| is_put(args).then(|| error.to_vec()));
            assert_eq!(reply, error);
            assert_eq!(
                rig.source(&[b"INF.PEEK", &rig.source]),
                before,
                "H3: refused destination destroyed source"
            );
        }
    }
}

/// A destination created after the read must win RENAMENX's NX condition.
#[test]
fn renamenx_racing_destination_preserves_both_values() {
    let rig = Rig::new(0);
    rig.seed();
    let reply = rig.run(CommandId::Renamenx, |args, rig| {
        if is_put(args) {
            assert_eq!(rig.local(1, &[b"SET", &rig.target, b"racer"]), b"+OK\r\n");
        }
        None
    });
    assert_eq!(reply, b":0\r\n", "H3: RENAMENX overwrote a racing destination");
    assert_eq!(rig.source(&[b"GET", &rig.source]), b"$7\r\npayroll\r\n");
    assert_eq!(rig.local(1, &[b"GET", &rig.target]), b"$5\r\nracer\r\n");
}

/// Mutations between snapshot and cleanup must survive; no stale unconditional DEL.
#[test]
fn source_replacement_is_not_deleted() {
    let rig = Rig::new(0);
    rig.seed();
    let reply = rig.run(CommandId::Rename, |args, rig| {
        if is_put(args) {
            assert_eq!(rig.source(&[b"SET", &rig.source, b"newer"]), b"+OK\r\n");
        }
        None
    });
    assert!(
        reply.starts_with(b"-BUSY source changed"),
        "H3: changed source reported success: {reply:?}"
    );
    assert_eq!(rig.source(&[b"GET", &rig.source]), b"$5\r\nnewer\r\n");
    assert_eq!(rig.local(1, &[b"GET", &rig.target]), b"$7\r\npayroll\r\n");
}

/// Cleanup transport failure reports failure and leaves the accepted copy readable.
#[test]
fn cleanup_failure_keeps_destination_copy() {
    let rig = Rig::new(1);
    rig.seed();
    let reply = rig.run(CommandId::Rename, |args, _| {
        (args[0] == b"INF.TAKE").then(|| b"-ERR cross-cell execution failed\r\n".to_vec())
    });
    assert_eq!(reply, b"-ERR cross-cell execution failed\r\n");
    assert_eq!(
        rig.local(0, &[b"GET", &rig.target]),
        b"$7\r\npayroll\r\n",
        "H3: cleanup failure has no destination copy"
    );
    assert_eq!(rig.source(&[b"GET", &rig.source]), b"$7\r\npayroll\r\n");
}

/// A full second of fabric delay cannot extend the source's absolute expiry.
#[test]
fn delayed_put_preserves_absolute_deadline() {
    let rig = Rig::new(0);
    rig.seed();
    let deadline = rig.source(&[b"PEXPIRETIME", &rig.source]);
    assert_eq!(
        rig.run(CommandId::Rename, |args, rig| {
            if is_put(args) {
                for plane in &rig.planes {
                    plane.shared.now.set(Nanos(1_000_000_000));
                }
            }
            None
        }),
        b"+OK\r\n"
    );
    assert_eq!(
        rig.local(1, &[b"PEXPIRETIME", &rig.target]),
        deadline,
        "H3: delayed move extended expiry"
    );
}

/// Changes of deadline, deletion, expiry and type all invalidate cleanup.
#[test]
fn source_deadline_deletion_and_expiry_invalidate_cleanup() {
    for mutation in [b"PEXPIRE".as_slice(), b"DEL", b"EXPIRE"] {
        let rig = Rig::new(0);
        rig.seed();
        let reply = rig.run(CommandId::Rename, |args, rig| {
            if is_put(args) {
                let mut change: Vec<&[u8]> = vec![mutation, &rig.source];
                if mutation != b"DEL" {
                    change.push(if mutation == b"EXPIRE" { b"0" } else { b"90000" });
                }
                assert_eq!(rig.source(&change), b":1\r\n");
            }
            None
        });
        assert!(reply.starts_with(b"-BUSY source changed"));
        if mutation == b"PEXPIRE" {
            assert_eq!(rig.source(&[b"PTTL", &rig.source]), b":90000\r\n");
        } else {
            assert_eq!(rig.source(&[b"GET", &rig.source]), b"$-1\r\n");
        }
    }
}

/// Successful moves work with either owner local, binary values, and no TTL.
#[test]
fn successful_moves_and_copies_both_directions() {
    for owner in [0, 1] {
        for command in [CommandId::Rename, CommandId::Renamenx, CommandId::Copy] {
            let rig = Rig::new(owner);
            assert_eq!(rig.source(&[b"SET", &rig.source, b"\0\xff\r\n"]), b"+OK\r\n");
            let reply = rig.run(command, |_, _| None);
            assert_eq!(
                reply,
                if command == CommandId::Rename { &b"+OK\r\n"[..] } else { b":1\r\n" }
            );
            assert_eq!(rig.local(1 - owner, &[b"GET", &rig.target]), b"$4\r\n\0\xff\r\n\r\n");
            assert_eq!(
                rig.source(&[b"EXISTS", &rig.source]),
                if command == CommandId::Copy { &b":1\r\n"[..] } else { b":0\r\n" }
            );
        }
    }
}

/// Only the exact positive acknowledgement authorizes cleanup.
#[test]
fn malformed_put_reply_never_authorizes_source_delete() {
    for response in [b"+OKAY\r\n".as_slice(), b":1\r\n", b"+OK\r\n+OK\r\n"] {
        let rig = Rig::new(0);
        rig.seed();
        let reply = rig.run(CommandId::Rename, |args, _| (is_put(args)).then(|| response.to_vec()));
        assert_eq!(reply, b"-ERR cross-cell program reply malformed\r\n");
        assert_eq!(rig.source(&[b"GET", &rig.source]), b"$7\r\npayroll\r\n");
    }
}

/// Redis admits `RENAME`/`RENAMENX` under `maxmemory` (neither is DENYOOM)
/// and refuses `COPY`; the destination leg carries the command's own
/// admission (ADR-0110 third amendment, batch 17). Pre-fix the leg was a
/// client-shaped `SET`, so every cross-cell rename into a cell at its
/// budget answered `-OOM` where the same-cell path and Redis succeed.
/// Arena exhaustion still refuses — the test below this one.
#[test]
fn destination_maxmemory_admits_renames_and_refuses_copy() {
    for owner in [0, 1] {
        for existing in [false, true] {
            for command in [CommandId::Rename, CommandId::Renamenx, CommandId::Copy] {
                let rig = Rig::new(owner);
                let target_cell = 1 - owner;
                rig.seed();
                if existing {
                    assert_eq!(
                        rig.local(
                            target_cell,
                            &[b"SET", &rig.target, b"previous", b"PX", b"900000"]
                        ),
                        b"+OK\r\n"
                    );
                }
                let source_deadline = rig.source(&[b"PEXPIRETIME", &rig.source]);
                let target_before = rig.local(target_cell, &[b"GET", &rig.target]);
                let target_deadline_before = rig.local(target_cell, &[b"PEXPIRETIME", &rig.target]);
                assert_eq!(
                    rig.local(
                        target_cell,
                        &[
                            b"CONFIG",
                            b"SET",
                            b"maxmemory",
                            b"1",
                            b"maxmemory-policy",
                            b"noeviction"
                        ]
                    ),
                    b"+OK\r\n"
                );
                // The gate is armed: a client SET at the destination refuses.
                assert!(rig.local(target_cell, &[b"SET", b"probe", b"v"]).starts_with(b"-OOM "));
                let reply = rig.run(command, |_, _| None);
                let label = format!("{command:?} owner={owner} existing={existing}");
                let lands = match (command, existing) {
                    (CommandId::Copy, _) => {
                        assert!(
                            reply.starts_with(b"-OOM "),
                            "{label}: COPY keeps DENYOOM: {reply:?}"
                        );
                        false
                    }
                    (CommandId::Renamenx, true) => {
                        assert_eq!(reply, b":0\r\n", "{label}");
                        false
                    }
                    (CommandId::Rename, _) => {
                        assert_eq!(
                            reply, b"+OK\r\n",
                            "{label}: Redis admits RENAME under maxmemory"
                        );
                        true
                    }
                    _ => {
                        assert_eq!(
                            reply, b":1\r\n",
                            "{label}: Redis admits RENAMENX under maxmemory"
                        );
                        true
                    }
                };
                if lands {
                    assert_eq!(rig.source(&[b"GET", &rig.source]), b"$-1\r\n", "{label}");
                    assert_eq!(
                        rig.local(target_cell, &[b"GET", &rig.target]),
                        b"$7\r\npayroll\r\n",
                        "{label}"
                    );
                    assert_eq!(
                        rig.local(target_cell, &[b"PEXPIRETIME", &rig.target]),
                        source_deadline,
                        "{label}: the absolute deadline travels with the value"
                    );
                } else {
                    assert_eq!(rig.source(&[b"GET", &rig.source]), b"$7\r\npayroll\r\n", "{label}");
                    assert_eq!(
                        rig.source(&[b"PEXPIRETIME", &rig.source]),
                        source_deadline,
                        "{label}"
                    );
                    assert_eq!(
                        rig.local(target_cell, &[b"GET", &rig.target]),
                        target_before,
                        "{label}"
                    );
                    assert_eq!(
                        rig.local(target_cell, &[b"PEXPIRETIME", &rig.target]),
                        target_deadline_before,
                        "{label}"
                    );
                }
            }
        }
    }
}

/// Arena exhaustion after the OOM admission gate is also non-destructive.
#[test]
fn arena_exhaustion_preserves_source_and_existing_destination() {
    for owner in [0, 1] {
        for existing in [false, true] {
            let rig = Rig::new(owner);
            rig.planes[1 - owner].shared.store.replace(Keyspace::new(StoreConfig {
                arena: inf_alloc::ArenaConfig { chunk_size: 65536, max_resident: Some(65536) },
                ..StoreConfig::default()
            }));
            let value = vec![b'v'; 65536];
            assert_eq!(rig.source(&[b"SET", &rig.source, &value]), b"+OK\r\n");
            if existing {
                assert_eq!(rig.local(1 - owner, &[b"SET", &rig.target, b"previous"]), b"+OK\r\n");
            }
            let before = rig.source(&[b"GET", &rig.source]);
            let target_before = rig.local(1 - owner, &[b"GET", &rig.target]);
            let reply = rig.run(CommandId::Rename, |_, _| None);
            assert!(reply.starts_with(b"-OOM "), "arena must actually refuse: {reply:?}");
            assert_eq!(
                rig.source(&[b"GET", &rig.source]),
                before,
                "H3: arena OOM destroyed source"
            );
            assert_eq!(rig.local(1 - owner, &[b"GET", &rig.target]), target_before);
        }
    }
}
