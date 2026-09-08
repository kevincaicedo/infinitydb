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
/// destination put is a client-shaped `SET … PXAT`, bound by SET's Redis
/// gate (M1, batch 16): the move refuses it and the source is preserved
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
            assert_eq!(
                reply, b"-ERR invalid expire time in 'set' command\r\n",
                "batch 16: a non-positive absolute deadline never reaches the store"
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
        if args[0] == b"SET" {
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
            let reply = rig.run(command, |args, _| (args[0] == b"SET").then(|| error.to_vec()));
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
        if args[0] == b"SET" {
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
        if args[0] == b"SET" {
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
            if args[0] == b"SET" {
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
            if args[0] == b"SET" {
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
        let reply =
            rig.run(CommandId::Rename, |args, _| (args[0] == b"SET").then(|| response.to_vec()));
        assert_eq!(reply, b"-ERR cross-cell program reply malformed\r\n");
        assert_eq!(rig.source(&[b"GET", &rig.source]), b"$7\r\npayroll\r\n");
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
