//! Command dispatch: the per-connection pump (pipeline order in, reply
//! order out, [`REMOTE_WINDOW`] remote ops in flight), the local fast path,
//! and `dispatch_one` — the command-family switch every remote command
//! enters through.

use super::*;

/// A reply slot awaiting its in-order turn on the wire.
pub(super) enum PendingReply {
    /// Executed (locally or refused) at dispatch; bytes wait their turn.
    Done(Vec<u8>),
    /// One remote `Apply` in flight; the owner's raw RESP reply parks in
    /// the gate if it lands before its turn.
    Remote { waiter: GateWait<u64, OwnedOutcome>, proto: Protocol },
    /// A tagged remote `PUBLISH` (ADR-0101 D4): the count reply, then the
    /// publisher's own frames stashed under `seq` by the tagged fan leg
    /// — Redis's reply-then-push order across a remote owner.
    Publish { waiter: GateWait<u64, OwnedOutcome>, proto: Protocol, seq: u64 },
    /// Split DEL/UNLINK/EXISTS/TOUCH (and scattered DBSIZE): locally-counted
    /// contributions in `acc`, remote per-key contributions in flight. A
    /// leg that answered a typed error instead of a count, or a leg that
    /// could not be sent (`refusal`), makes the reply that error — never
    /// a partial sum (review of 2026-08-28, M4.5-S37 finding 2).
    Counted {
        waiters: Vec<GateWait<u64, OwnedOutcome>>,
        acc: i64,
        proto: Protocol,
        refusal: Option<Vec<u8>>,
    },
    /// Split MGET / JSON.MGET: per-key replies reassemble into one array
    /// in argv order. `unwrap_single` marks JSON.MGET's shape: each
    /// sub-reply is a single-key `*1` array whose element joins the outer
    /// array (ADR-0041 D9).
    Gather { parts: Vec<GatherPart>, proto: Protocol, unwrap_single: bool },
    /// Fanned MSET / scattered FLUSH: all legs must come back `+OK` (the
    /// first error leg wins the reply otherwise). A leg that could not be
    /// sent (`refusal`) makes the reply that refusal — never a silent
    /// `+OK` over a dropped cell (review of 2026-08-30, H1 / F-L13-01);
    /// already-sent waiters still drain so nothing orphans in flight.
    AllOk { waiters: Vec<GateWait<u64, OwnedOutcome>>, proto: Protocol, refusal: Option<Vec<u8>> },
    /// `always` durable write (M2-S08): the reply bytes are staged but the
    /// slot resolves only once the fsync watermark covers the record's
    /// durable seq (§8.2 — ack after fsync; FIFO per connection holds).
    Durable { waiter: inf_runtime::WatermarkWait, reply: Vec<u8> },
}

/// What the pump found when it asked the connection for more work.
pub(super) enum Popped {
    Cmd(OwnedCmd),
    /// Queue empty but replies are still pending — keep emitting.
    Empty,
    /// Queue empty, nothing pending (pump deactivated inside the conn
    /// borrow) or the connection is gone: the pump is done.
    Finished,
}

fn pop_or_quiesce<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Shared<O, F>,
    key: ConnKey,
    pending_empty: bool,
) -> Popped {
    let Some(next) = shared.with_conn(key, |conn| {
        let next = conn.queue.pop_front();
        if next.is_none() && pending_empty {
            conn.pump_active = false;
        }
        if conn.recv_disarmed && conn.queue.len() <= PENDING_LOW_WATER {
            conn.rearm_recv = true;
        }
        next
    }) else {
        return Popped::Finished;
    };
    match next {
        Some(cmd) => Popped::Cmd(cmd),
        None if pending_empty => Popped::Finished,
        None => Popped::Empty,
    }
}

/// Commands that mutate connection execution state must observe — and be
/// observed by — their exact pipeline position (HELLO switches the protocol
/// every later reply serializes under; SELECT switches the database every
/// later command routes to — M1-S08).
fn is_conn_state(owned: &OwnedCmd) -> bool {
    lookup(owned.arg(0)).is_some_and(|m| match m.id {
        CommandId::Hello | CommandId::Select => true,
        // `INF.NS USE` switches the namespace every later command routes
        // to — the SELECT barrier class (M2-S08).
        CommandId::InfNs => owned.argc() > 1 && owned.arg(1).eq_ignore_ascii_case(b"USE"),
        _ => false,
    })
}

/// Outcome of the de-async dispatch fast path (ADR-0030 D4).
enum FastDispatch {
    /// Handled synchronously; `pending`/`inflight` updated.
    Handled,
    /// The connection is gone; the pump exits.
    ConnGone,
    /// Not a fast arm (rare shape) or no fabric credit on the first send
    /// attempt: run the async [`dispatch_one`] — the unchanged slow path.
    Fallback,
}

/// The pump's synchronous dispatch fast path (M2.5 Phase H, ADR-0030 D4):
/// the arms that dominate the natural-routing mix — the single-owner
/// remote `Apply` and the local mirror — dispatch without constructing
/// the [`dispatch_one`] future, whose send path suspends only on
/// fabric-credit exhaustion. Guard order mirrors `dispatch_one`'s match
/// arms exactly; every other shape falls back to it
/// (`deasync_dispatch_matches_pump_semantics` pins the equivalence).
fn dispatch_one_fast<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    key: ConnKey,
    owned: &OwnedCmd,
    pending: &mut VecDeque<PendingReply>,
    inflight: &mut usize,
) -> FastDispatch {
    let argc = owned.argc();
    if argc > ARGV_INLINE {
        // Wide argv (MSET…): rare — keep the heap-argv path async.
        return FastDispatch::Fallback;
    }
    let mut argv_inline: [&[u8]; ARGV_INLINE] = [b""; ARGV_INLINE];
    for (i, slot) in argv_inline[..argc].iter_mut().enumerate() {
        *slot = owned.arg(i);
    }
    let argv: &[&[u8]] = &argv_inline[..argc];
    let Some((proto, id, db, conn_ns, ns_unavailable, restricted)) = shared.with_conn(key, |c| {
        (
            c.cx.proto,
            c.cx.id,
            c.cx.db,
            c.cx.ns.named(),
            c.cx.ns.unavailable(),
            pubsub::subscriber_restricted(&c.cx),
        )
    }) else {
        return FastDispatch::ConnGone;
    };
    let origin = ExecOrigin::Conn(key.slot, key.generation);
    let meta = lookup(argv[0]);
    let well_formed = meta.is_some_and(|m| arity_ok(m, argv.len()));
    // ADR-0115: a fabric-program primitive typed by a client is unknown —
    // refused here, before any routing, so it never crosses the fabric.
    if let Some(m) = meta
        && m.flags.contains(CmdFlags::INTERNAL)
    {
        let mut reply = shared.take_reply_buf();
        crate::exec::unknown_command_reply(argv, proto, &mut reply);
        pending.push_back(PendingReply::Done(reply));
        return FastDispatch::Handled;
    }
    if let Some(meta) = meta
        && well_formed
        && ns_unavailable
        && !unavailable_default_allows(meta.id)
    {
        pending.push_back(PendingReply::Done(unavailable_default_reply(proto)));
        return FastDispatch::Handled;
    }
    if let Some(meta) = meta
        && well_formed
        && restricted
        && !pubsub::allowed_in_subscriber_mode(meta.id)
    {
        // The gate must use the Redis allowlist, not "commands the plane
        // routes to the pub/sub engine": `PUBSUB`/`PUBLISH` are plane
        // pub/sub yet restricted in RESP2 subscriber mode — the old
        // predicate let them through here while `execute`'s fast path
        // refused them (found 2026-09-01 by the INFINITYD_BIN lane).
        pending.push_back(PendingReply::Done(restricted_reply(shared, meta, argv, proto)));
        return FastDispatch::Handled;
    }
    if let Some(m) = meta
        && well_formed
    {
        // The program/rare arms, in dispatch_one's guard order: pub/sub,
        // NS DDL, ckpt surface, named-namespace dispatch, scatter.
        if pubsub::is_plane_pubsub(m.id)
            || (m.id == CommandId::InfNs && is_ns_ddl_sub(argv.get(1).copied()))
            || matches!(m.id, CommandId::InfCkpt | CommandId::Bgsave | CommandId::Lastsave)
            || (conn_ns.is_some() && !is_conn_state(owned))
            || (is_scatter(m.id, argv.get(1).copied())
                && shared.cells > 1
                && !shared.route_local_only)
        {
            return FastDispatch::Fallback;
        }
        // One routing pass per command, as in dispatch_one.
        let mut first_owner = shared.cell;
        let mut any_remote = false;
        if !shared.route_local_only {
            let mut first = true;
            for k in extract_keys_iter(m, argv) {
                let owner = shared.router.cell_of(SlotRouter::slot_of(k));
                if first {
                    first_owner = owner;
                    first = false;
                }
                if owner != shared.cell {
                    any_remote = true;
                    break;
                }
            }
        }
        if any_remote {
            let split = matches!(
                m.id,
                CommandId::Del
                    | CommandId::Exists
                    | CommandId::Unlink
                    | CommandId::Touch
                    | CommandId::Mget
                    | CommandId::Mset
                    | CommandId::Msetnx
            );
            let two_owner =
                matches!(m.id, CommandId::Rename | CommandId::Renamenx | CommandId::Copy)
                    && shared.router.cell_of(SlotRouter::slot_of(argv[1]))
                        != shared.router.cell_of(SlotRouter::slot_of(argv[2]));
            if split || two_owner {
                return FastDispatch::Fallback;
            }
            // Single-owner remote command: the hot arm.
            return match try_send_apply(shared, first_owner, ApplyOrigin::Client, proto, db, argv) {
                SendNow::Sent(waiter) => {
                    *inflight += 1;
                    pending.push_back(PendingReply::Remote { waiter, proto });
                    FastDispatch::Handled
                }
                SendNow::Refused(refusal) => {
                    pending.push_back(PendingReply::Done(refusal));
                    FastDispatch::Handled
                }
                SendNow::NoCredit => FastDispatch::Fallback,
            };
        }
    }
    if dispatch_mirror(shared, key, owned, argv, origin, proto, id, db, conn_ns, pending) {
        FastDispatch::Handled
    } else {
        FastDispatch::ConnGone
    }
}

/// The per-connection pump: dispatch commands in pipeline order with up to
/// [`REMOTE_WINDOW`] remote ops in flight, emit replies strictly in command
/// order. Suspends only on the front reply's gate and on fabric credits;
/// out-of-order completions park in the gate until their turn.
pub(super) async fn pump<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: Rc<Shared<O, F>>,
    key: ConnKey,
    first: OwnedCmd,
) {
    let mut pending: VecDeque<PendingReply> = VecDeque::new();
    // Remote ops sent and not yet awaited (Counted holds several).
    let mut inflight: usize = 0;
    // A command held back by the conn-state barrier.
    let mut held: Option<OwnedCmd> = Some(first);
    loop {
        // ---- dispatch: fill the window in pipeline order.
        while pending.len() < PENDING_REPLIES_MAX && inflight < REMOTE_WINDOW {
            let cmd = match held.take() {
                Some(cmd) => cmd,
                None => match pop_or_quiesce(&shared, key, pending.is_empty()) {
                    Popped::Cmd(cmd) => cmd,
                    Popped::Empty => break,
                    Popped::Finished => return,
                },
            };
            if is_conn_state(&cmd) && !pending.is_empty() {
                held = Some(cmd);
                break;
            }
            // De-async fast path (ADR-0030 D4): hot arms dispatch without
            // constructing the `dispatch_one` future; rare shapes and
            // credit exhaustion fall back to the async path unchanged.
            let handled = if shared.deasync_dispatch.get() {
                match dispatch_one_fast(&shared, key, &cmd, &mut pending, &mut inflight) {
                    FastDispatch::Handled => true,
                    FastDispatch::ConnGone => return,
                    FastDispatch::Fallback => false,
                }
            } else {
                false
            };
            if !handled && !dispatch_one(&shared, key, &cmd, &mut pending, &mut inflight).await {
                return; // connection is gone
            }
            // The command's flat buffer recycles once dispatched (waiters
            // own their inputs via `ApplyArgs`/reply slots — nothing
            // borrows `cmd` past this point).
            shared.recycle_cmd_buf(cmd.into_buf());
        }

        // ---- emit: resolve the front reply. Awaiting an already-parked
        // value completes on first poll; only a genuinely outstanding front
        // suspends the pump.
        let Some(front) = pending.pop_front() else {
            continue; // barrier held with pending drained: dispatch it now
        };
        let reply: Vec<u8> = match front {
            PendingReply::Done(bytes) => bytes,
            PendingReply::Remote { waiter, proto } => {
                let outcome = waiter.await;
                inflight -= 1;
                render_outcome(&shared, outcome, proto)
            }
            PendingReply::Publish { waiter, proto, seq } => {
                let outcome = waiter.await;
                inflight -= 1;
                let mut reply = render_outcome(&shared, outcome, proto);
                // The owner replied only after this cell acked its fan
                // leg, so a self-frame stash for `seq` is either already
                // here or never existed (the publisher was not subscribed
                // at delivery) — ADR-0101 D4.
                if let Some(frames) =
                    shared.with_conn(key, |conn| conn.take_self_push(seq)).flatten()
                {
                    reply.extend_from_slice(&frames);
                }
                reply
            }
            PendingReply::Counted { waiters, acc, proto, refusal } => {
                // Every leg is awaited so nothing is orphaned in flight;
                // the fold decides the reply (a count, or the first
                // error — a leg that is not a count is an error, never
                // a partial sum: ADR-0093 A3′).
                let mut fold = CountedFold::new(acc, refusal);
                for waiter in waiters {
                    if let Some(unused) = fold.leg(waiter.await, proto) {
                        shared.recycle_reply_buf(unused);
                    }
                    inflight -= 1;
                }
                match fold.finish() {
                    Err(error) => return_error(&shared, error, proto),
                    Ok(total) => {
                        let mut reply = shared.take_reply_buf();
                        RespWriter::new(&mut reply, proto).int(total);
                        reply
                    }
                }
            }
            PendingReply::Gather { parts, proto, unwrap_single } => {
                let mut reply = shared.take_reply_buf();
                RespWriter::new(&mut reply, proto).array_header(parts.len());
                for part in parts {
                    match part {
                        GatherPart::Done(bytes) => {
                            let element =
                                if unwrap_single { strip_single_element(&bytes) } else { &bytes };
                            reply.extend_from_slice(element);
                            shared.recycle_reply_buf(bytes);
                        }
                        GatherPart::Wait(waiter) => {
                            let outcome = waiter.await;
                            inflight -= 1;
                            match outcome {
                                OwnedOutcome::Bytes(bytes) => {
                                    let element = if unwrap_single {
                                        strip_single_element(&bytes)
                                    } else {
                                        &bytes
                                    };
                                    reply.extend_from_slice(element);
                                    shared.recycle_reply_buf(bytes);
                                }
                                _ => RespWriter::new(&mut reply, proto).null(),
                            }
                        }
                    }
                }
                reply
            }
            PendingReply::Durable { waiter, reply } => {
                waiter.await;
                reply
            }
            PendingReply::AllOk { waiters, proto, refusal } => {
                // A refusal (a leg never sent) outranks leg errors: the
                // fan is known-incomplete, and `+OK` would be a lie.
                let mut failure: Option<Vec<u8>> = refusal;
                for waiter in waiters {
                    let outcome = waiter.await;
                    inflight -= 1;
                    if failure.is_none()
                        && let OwnedOutcome::Bytes(bytes) = &outcome
                        && bytes.first() == Some(&b'-')
                    {
                        failure = Some(bytes.clone());
                    }
                }
                match failure {
                    Some(error) => error,
                    None => {
                        let mut reply = shared.take_reply_buf();
                        RespWriter::new(&mut reply, proto).simple("OK");
                        reply
                    }
                }
            }
        };
        let written = shared.with_conn(key, |conn| conn.out.extend_from_slice(&reply));
        shared.recycle_reply_buf(reply);
        if written.is_none() {
            return;
        }
    }
}

/// Keyspace-wide commands that must scatter across all cells on a
/// multi-cell node (M1-S02). `sub` is argv[1] when present: CONFIG SET /
/// RESETSTAT and INF.NS CREATE / DROP mutate per-cell state (typed config,
/// namespace registries — M1-E3/E4) and fan out AllOk; their read forms
/// stay local.
pub(super) fn is_scatter(id: CommandId, sub: Option<&[u8]>) -> bool {
    match id {
        CommandId::Dbsize
        | CommandId::Keys
        | CommandId::Scan
        | CommandId::Flushdb
        | CommandId::Flushall
        | CommandId::Randomkey => true,
        CommandId::Config => sub.is_some_and(|s| {
            s.eq_ignore_ascii_case(b"SET") || s.eq_ignore_ascii_case(b"RESETSTAT")
        }),
        // INF.NS DDL is a program (`program_ns_ddl`), routed ahead of the
        // scatter arm on every node shape — never a scatter (L13 style
        // row: the scatter arm's `InfNs` case was dead).
        _ => false,
    }
}

/// Peer legs for one scatter-mutator fan. A variadic `CONFIG SET` chunks
/// its pairs so every leg stays under the fabric codec's `MAX_APPLY_ARGS`
/// (review of 2026-08-30, H1 / F-L13-01: at ≥ 8 pairs every peer leg was
/// refused at the encoder and silently dropped — `+OK` with the peers
/// never told); every other mutator fans its argv whole (bounded arity).
/// Chunks land on each peer in ring-FIFO order and re-validate against
/// the same config table, so a locally-validated command cannot fail a
/// chunk; cross-chunk atomicity per peer matches the fan's existing
/// cross-cell eventual semantics (ADR-0098).
fn scatter_fan_legs<'a>(id: CommandId, argv: &[&'a [u8]]) -> Vec<Vec<&'a [u8]>> {
    const PAIRS_PER_LEG: usize = (MAX_APPLY_ARGS - 2) / 2;
    if id == CommandId::Config
        && argv.len() > MAX_APPLY_ARGS
        && argv.get(1).is_some_and(|s| s.eq_ignore_ascii_case(b"SET"))
    {
        return argv[2..]
            .chunks(2 * PAIRS_PER_LEG)
            .map(|pairs| {
                let mut leg = Vec::with_capacity(2 + pairs.len());
                leg.push(argv[0]);
                leg.push(argv[1]);
                leg.extend_from_slice(pairs);
                leg
            })
            .collect();
    }
    vec![argv.to_vec()]
}

/// `INF.NS` subcommands that ride the pump's DDL program: CREATE/DROP
/// since M2-S08; SET since M4-S19 (hot-reload mutates registries on
/// every cell and persists the catalog — DDL semantics exactly).
fn is_ns_ddl_sub(sub: Option<&[u8]>) -> bool {
    sub.is_some_and(|s| {
        s.eq_ignore_ascii_case(b"CREATE")
            || s.eq_ignore_ascii_case(b"DROP")
            || s.eq_ignore_ascii_case(b"SET")
    })
}

/// Dispatch one command: execute locally into a `Done` slot, or ship its
/// remote ops (suspending only on fabric credits — backpressure, never
/// unbounded queueing) and stage the reply waiter. Multi-key commands split
/// per key; RENAME/RENAMENX/COPY across two owners and keyspace-wide
/// commands run as inline fabric programs (M1-S02). Returns `false` when
/// the connection is gone.
async fn dispatch_one<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    key: ConnKey,
    owned: &OwnedCmd,
    pending: &mut VecDeque<PendingReply>,
    inflight: &mut usize,
) -> bool {
    // Argv views live on the stack for the common arity; wide commands
    // (MSET…) fall back to `slices()` (M2.5 Phase H allocator lever).
    let argc = owned.argc();
    let mut argv_inline: [&[u8]; ARGV_INLINE] = [b""; ARGV_INLINE];
    let argv_heap: Vec<&[u8]>;
    let argv: &[&[u8]] = if argc <= ARGV_INLINE {
        for (i, slot) in argv_inline[..argc].iter_mut().enumerate() {
            *slot = owned.arg(i);
        }
        &argv_inline[..argc]
    } else {
        argv_heap = owned.slices();
        &argv_heap
    };
    // One slab lookup per command: execution context + subscriber
    // restriction together (was two separate `with_conn` walks).
    let Some((proto, id, db, conn_ns, ns_unavailable, restricted)) = shared.with_conn(key, |c| {
        (
            c.cx.proto,
            c.cx.id,
            c.cx.db,
            c.cx.ns.named(),
            c.cx.ns.unavailable(),
            pubsub::subscriber_restricted(&c.cx),
        )
    }) else {
        return false;
    };
    let origin = ExecOrigin::Conn(key.slot, key.generation);

    let meta = lookup(argv[0]);
    let well_formed = meta.is_some_and(|m| arity_ok(m, argv.len()));
    // ADR-0115: as on the fast path — unknown before any routing.
    if let Some(m) = meta
        && m.flags.contains(CmdFlags::INTERNAL)
    {
        let mut reply = shared.take_reply_buf();
        crate::exec::unknown_command_reply(argv, proto, &mut reply);
        pending.push_back(PendingReply::Done(reply));
        return true;
    }
    if let Some(meta) = meta
        && well_formed
        && ns_unavailable
        && !unavailable_default_allows(meta.id)
    {
        pending.push_back(PendingReply::Done(unavailable_default_reply(proto)));
        return true;
    }
    // M1-S10: RESP2 subscriber-mode restriction for pump-dispatched
    // commands — the fast path checks inside `execute`, but commands
    // landing here would otherwise run under a synthesized ConnCx without
    // the subscription state (remote Apply, scatter legs). The predicate
    // is the Redis allowlist, not `is_plane_pubsub`: `PUBSUB`/`PUBLISH`
    // are plane pub/sub yet restricted in RESP2 subscriber mode (found
    // 2026-09-01 by the INFINITYD_BIN lane, matrix case `PUBSUB
    // CHANNELS`).
    if let Some(meta) = meta
        && well_formed
        && restricted
        && !pubsub::allowed_in_subscriber_mode(meta.id)
    {
        pending.push_back(PendingReply::Done(restricted_reply(shared, meta, argv, proto)));
        return true;
    }
    // One routing pass per command (M2.5 Phase H: was one
    // `extract_keys_slices` Vec per match guard plus a second — and a
    // second `slot_of` — inside the single-owner arm): the first key, its
    // owner, and remote presence, computed together. The pass stops at the
    // first remote key; the first key is always index 0 of the spec, so
    // `first_owner` is captured before any early exit.
    let mut first_owner = shared.cell;
    let mut any_remote = false;
    if let Some(m) = meta
        && well_formed
        && !shared.route_local_only
    {
        let mut first = true;
        for k in extract_keys_iter(m, argv) {
            let owner = shared.router.cell_of(SlotRouter::slot_of(k));
            if first {
                first_owner = owner;
                first = false;
            }
            if owner != shared.cell {
                any_remote = true;
                break;
            }
        }
    }
    let has_remote_key = |_meta| any_remote;
    let owner_of = |k: &[u8]| shared.router.cell_of(SlotRouter::slot_of(k));
    match meta {
        Some(meta) if well_formed && pubsub::is_plane_pubsub(meta.id) => {
            return dispatch_pubsub(shared, key, meta.id, argv, proto, pending, inflight).await;
        }
        // Namespace DDL (M2-S08, ADR-0015 D2/D3): id allocation, local
        // apply, peer fan, catalog persist — on every node shape (1-cell
        // included), which is why `needs_fabric` routes all INF.NS here.
        Some(meta)
            if well_formed
                && meta.id == CommandId::InfNs
                && is_ns_ddl_sub(argv.get(1).copied()) =>
        {
            let reply = program_ns_ddl(shared, origin, proto, id, db, argv).await;
            pending.push_back(PendingReply::Done(reply));
        }
        // Checkpoint operator surface (M2-S20, ADR-0021 D6).
        Some(meta)
            if well_formed
                && matches!(
                    meta.id,
                    CommandId::InfCkpt | CommandId::Bgsave | CommandId::Lastsave
                ) =>
        {
            let reply = program_ckpt(shared, proto, meta.id, argv).await;
            pending.push_back(PendingReply::Done(reply));
        }
        // Named-namespace commands (M2-S08): single-owner shape — local
        // execution with durable admission/emission/gating, or a whole-argv
        // `ApplyNs` to the owning cell. Conn-state (SELECT/HELLO/USE) falls
        // through to the mirror arm below. `CONFIG SET`/`RESETSTAT` are
        // node-wide hot-per-cell keys whatever namespace the connection
        // selected — they belong to the scatter arm below (review of
        // 2026-08-27, M4.5-S37: a namespace-bound connection applied them
        // to its own cell only, found by the `m4-tiered` DST's per-cell
        // witness on `tiered-shadow-overwrite`). `FLUSHALL` is node-wide
        // whatever the connection selected, for the same reason (review
        // of 2026-08-30, C1: it replied `+OK` having flushed one cell of
        // `cells`); `FLUSHDB` stays here — under a named namespace it
        // means "flush the namespace", which `execute` refuses typed
        // (ADR-0015). `SCAN`/`KEYS`/`RANDOMKEY` stay here too:
        // `dispatch_ns` scatters them namespace-aware.
        Some(meta)
            if well_formed
                && conn_ns.is_some()
                && !is_conn_state(owned)
                && !(matches!(meta.id, CommandId::Config | CommandId::Flushall)
                    && is_scatter(meta.id, argv.get(1).copied())) =>
        {
            let ns = conn_ns.expect("guarded");
            return dispatch_ns(
                shared, key, origin, meta, argv, proto, id, db, ns, pending, inflight,
            )
            .await;
        }
        Some(meta)
            if well_formed
                && is_scatter(meta.id, argv.get(1).copied())
                && shared.cells > 1
                && !shared.route_local_only =>
        {
            pending.push_back(
                dispatch_scatter(shared, meta.id, origin, proto, id, db, argv, inflight).await,
            );
        }
        Some(meta)
            if well_formed
                && matches!(
                    meta.id,
                    CommandId::Del | CommandId::Exists | CommandId::Unlink | CommandId::Touch
                )
                && has_remote_key(meta) =>
        {
            // Per-key split: local keys count at dispatch, remote keys ride
            // typed Apply replies. Applies leave in argv order (per-key
            // order rides the destination ring FIFO).
            let name: &[u8] = argv[0];
            let mut acc: i64 = 0;
            let mut waiters = Vec::new();
            for k in &argv[1..] {
                if shared.router.is_local(k, shared.cell) {
                    acc += shared.apply_counted(origin, name, k, db);
                } else {
                    match send_apply(
                        shared,
                        owner_of(k),
                        ApplyOrigin::Client,
                        proto,
                        db,
                        &[name, k],
                    )
                    .await
                    {
                        Ok(waiter) => {
                            waiters.push(waiter);
                            *inflight += 1;
                        }
                        Err(_) => debug_assert!(false, "2-arg apply exceeded ApplyArgs"),
                    }
                }
            }
            pending.push_back(PendingReply::Counted { waiters, acc, proto, refusal: None });
        }
        Some(meta) if well_formed && meta.id == CommandId::Mget && has_remote_key(meta) => {
            pending.push_back(gather_mget(shared, origin, proto, id, db, argv, inflight).await);
        }
        #[cfg(feature = "doc")]
        Some(meta) if well_formed && meta.id == CommandId::JsonMget && has_remote_key(meta) => {
            pending
                .push_back(gather_json_mget(shared, origin, proto, id, db, argv, inflight).await);
        }
        Some(meta) if well_formed && meta.id == CommandId::Mset && has_remote_key(meta) => {
            pending.push_back(program_mset(shared, origin, proto, id, db, argv, inflight).await);
        }
        Some(meta) if well_formed && meta.id == CommandId::Msetnx && has_remote_key(meta) => {
            let reply = program_msetnx(shared, origin, proto, id, db, argv).await;
            pending.push_back(PendingReply::Done(reply));
        }
        Some(meta)
            if well_formed
                && matches!(meta.id, CommandId::Rename | CommandId::Renamenx | CommandId::Copy)
                && has_remote_key(meta)
                && owner_of(argv[1]) != owner_of(argv[2]) =>
        {
            // Two owners: the read(+delete)/write fabric program. Same-owner
            // pairs fall through to the whole-argv Apply below (atomic at
            // that cell).
            let reply = program_move(shared, origin, proto, id, db, meta.id, argv).await;
            pending.push_back(PendingReply::Done(reply));
        }
        Some(meta) if well_formed && has_remote_key(meta) => {
            // Single-owner remote command: ship the whole argv; the owner
            // executes and returns its raw RESP reply. The destination is
            // the first key's owner, computed in the routing pass above.
            let owner = first_owner;
            match send_apply(shared, owner, ApplyOrigin::Client, proto, db, argv).await {
                Ok(waiter) => {
                    *inflight += 1;
                    pending.push_back(PendingReply::Remote { waiter, proto });
                }
                Err(refusal) => pending.push_back(PendingReply::Done(refusal)),
            }
        }
        _ => {
            return dispatch_mirror(
                shared, key, owned, argv, origin, proto, id, db, conn_ns, pending,
            );
        }
    }
    true
}

/// The keyspace-wide scatter programs of `dispatch_one` (`is_scatter`):
/// a counted `DBSIZE`, the all-or-nothing per-cell mutators, and the
/// `KEYS`/`SCAN`/`RANDOMKEY` folds. Returns the reply slot to stage.
#[allow(clippy::too_many_arguments, reason = "the dispatch context, not an API surface")]
async fn dispatch_scatter<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    cmd: CommandId,
    origin: ExecOrigin,
    proto: Protocol,
    id: u64,
    db: u16,
    argv: &[&[u8]],
    inflight: &mut usize,
) -> PendingReply {
    match cmd {
        CommandId::Dbsize => {
            let acc = shared.apply_dbsize(origin, db);
            let mut waiters = Vec::new();
            let mut refusal = None;
            for cell in peer_cells(shared) {
                // A leg that cannot be sent becomes the reply —
                // never a partial sum (review of 2026-08-30, H1:
                // `if let Ok` silently dropped the cell).
                match send_apply(shared, cell, ApplyOrigin::Client, proto, db, &[b"DBSIZE"]).await {
                    Ok(waiter) => {
                        waiters.push(waiter);
                        *inflight += 1;
                    }
                    Err(reply) => {
                        refusal = Some(reply);
                        break;
                    }
                }
            }
            PendingReply::Counted { waiters, acc, proto, refusal }
        }
        CommandId::Flushdb | CommandId::Flushall | CommandId::Config => {
            // Per-cell-state mutators (flush, CONFIG SET/RESETSTAT):
            // the local leg validates and applies; an error reply
            // short-circuits the fan-out.
            // Every command on this arm must therefore be
            // all-or-nothing locally (review of 2026-08-30, H1 /
            // F-L17-12: `CONFIG SET` was not — ADR-0098 split it).
            let local = run_local(shared, origin, proto, id, db, argv);
            if local.first() == Some(&b'-') {
                PendingReply::Done(local)
            } else {
                shared.recycle_reply_buf(local);
                // A wide `CONFIG SET` fans in pair-preserving
                // chunks under the codec's argument cap; a leg
                // that still cannot be sent surfaces as the reply
                // instead of being swallowed (H1 / F-L13-01: 8+
                // pairs answered `+OK` while every peer leg was
                // silently refused).
                let legs = scatter_fan_legs(cmd, argv);
                let mut waiters = Vec::new();
                let mut refusal = None;
                'fan: for cell in peer_cells(shared) {
                    for leg in &legs {
                        match send_apply(shared, cell, ApplyOrigin::Client, proto, db, leg).await {
                            Ok(waiter) => {
                                waiters.push(waiter);
                                *inflight += 1;
                            }
                            Err(reply) => {
                                refusal = Some(reply);
                                break 'fan;
                            }
                        }
                    }
                }
                PendingReply::AllOk { waiters, proto, refusal }
            }
        }
        CommandId::Keys => {
            let reply = program_keys(shared, origin, proto, id, db, ScatterScope::Db, argv).await;
            PendingReply::Done(reply)
        }
        CommandId::Scan => {
            let reply = program_scan(shared, origin, proto, id, db, ScatterScope::Db, argv).await;
            PendingReply::Done(reply)
        }
        CommandId::Randomkey => {
            let reply =
                program_randomkey(shared, origin, proto, id, db, ScatterScope::Db, argv).await;
            PendingReply::Done(reply)
        }
        _ => unreachable!("is_scatter covers exactly the arms above"),
    }
}

/// `MGET` with a remote key: every position resolves independently and
/// the replies reassemble into one array in argv order.
async fn gather_mget<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    origin: ExecOrigin,
    proto: Protocol,
    id: u64,
    db: u16,
    argv: &[&[u8]],
    inflight: &mut usize,
) -> PendingReply {
    // Gather: every position resolves independently, replies
    // reassemble into one array in argv order (the §6.2 "pipelined,
    // not serialized" shape; per-destination BatchOp coalescing is
    // the M1-S17 story).
    let mut parts = Vec::with_capacity(argv.len() - 1);
    for k in &argv[1..] {
        if shared.router.is_local(k, shared.cell) {
            let mut buf = shared.take_reply_buf();
            shared.execute_owned_into(origin, &[b"GET", k], proto, id, db, None, false, &mut buf);
            parts.push(GatherPart::Done(buf));
        } else {
            match send_apply(
                shared,
                shared.router.cell_of(SlotRouter::slot_of(k)),
                ApplyOrigin::Client,
                proto,
                db,
                &[b"GET", k],
            )
            .await
            {
                Ok(waiter) => {
                    parts.push(GatherPart::Wait(waiter));
                    *inflight += 1;
                }
                Err(refusal) => parts.push(GatherPart::Done(refusal)),
            }
        }
    }
    PendingReply::Gather { parts, proto, unwrap_single: false }
}

/// `JSON.MGET` with a remote key (M3-S11; ADR-0041 D9): the `MGET` shape
/// with single-key sub-ops, the path riding every one.
#[cfg(feature = "doc")]
async fn gather_json_mget<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    origin: ExecOrigin,
    proto: Protocol,
    id: u64,
    db: u16,
    argv: &[&[u8]],
    inflight: &mut usize,
) -> PendingReply {
    // JSON.MGET gather (M3-S11; ADR-0041 D9): the MGET shape with
    // single-key `JSON.MGET k path` sub-ops — each sub-reply is a
    // `*1` array whose element joins the outer array in argv
    // order. The path (final argument) rides every sub-op.
    let path = argv[argv.len() - 1];
    let mut parts = Vec::with_capacity(argv.len() - 2);
    for k in &argv[1..argv.len() - 1] {
        if shared.router.is_local(k, shared.cell) {
            let mut buf = shared.take_reply_buf();
            let sub: [&[u8]; 3] = [b"JSON.MGET", k, path];
            shared.execute_owned_into(origin, &sub, proto, id, db, None, false, &mut buf);
            parts.push(GatherPart::Done(buf));
        } else {
            let sub: [&[u8]; 3] = [b"JSON.MGET", k, path];
            match send_apply(
                shared,
                shared.router.cell_of(SlotRouter::slot_of(k)),
                ApplyOrigin::Client,
                proto,
                db,
                &sub,
            )
            .await
            {
                Ok(waiter) => {
                    parts.push(GatherPart::Wait(waiter));
                    *inflight += 1;
                }
                Err(refusal) => parts.push(GatherPart::Done(refusal)),
            }
        }
    }
    PendingReply::Gather { parts, proto, unwrap_single: true }
}

/// `MSET` with a remote key: local pairs first (an OOM reply preempts the
/// fan), then one `SET` leg per remote pair under an all-OK fold.
async fn program_mset<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    origin: ExecOrigin,
    proto: Protocol,
    id: u64,
    db: u16,
    argv: &[&[u8]],
    inflight: &mut usize,
) -> PendingReply {
    if argv.len().is_multiple_of(2) {
        PendingReply::Done(error_reply(
            shared,
            proto,
            "ERR wrong number of arguments for 'mset' command",
        ))
    } else {
        // Local pairs first (an OOM error reply preempts the fan).
        let mut failure: Option<Vec<u8>> = None;
        let mut i = 1;
        while i < argv.len() {
            if shared.router.is_local(argv[i], shared.cell) {
                let mut buf = shared.take_reply_buf();
                shared.execute_owned_into(
                    origin,
                    &[b"SET", argv[i], argv[i + 1]],
                    proto,
                    id,
                    db,
                    None,
                    false,
                    &mut buf,
                );
                if buf.first() == Some(&b'-') && failure.is_none() {
                    failure = Some(buf);
                } else {
                    shared.recycle_reply_buf(buf);
                }
            }
            i += 2;
        }
        if let Some(error) = failure {
            PendingReply::Done(error)
        } else {
            let mut waiters = Vec::new();
            let mut i = 1;
            while i < argv.len() {
                if !shared.router.is_local(argv[i], shared.cell)
                    && let Ok(waiter) = send_apply(
                        shared,
                        shared.router.cell_of(SlotRouter::slot_of(argv[i])),
                        ApplyOrigin::Client,
                        proto,
                        db,
                        &[b"SET", argv[i], argv[i + 1]],
                    )
                    .await
                {
                    waiters.push(waiter);
                    *inflight += 1;
                }
                i += 2;
            }
            PendingReply::AllOk { waiters, proto, refusal: None }
        }
    }
}

/// The RESP2 subscriber-restriction reply for a pump-dispatched command
/// (M1-S10) — shared verbatim by [`dispatch_one`] and the de-async fast
/// path (ADR-0030 D4): both arms must produce identical bytes.
fn restricted_reply<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Shared<O, F>,
    meta: &'static inf_wire::CommandMeta,
    argv: &[&[u8]],
    proto: Protocol,
) -> Vec<u8> {
    let mut reply = shared.take_reply_buf();
    if meta.id == CommandId::Ping {
        if argv.len() > 2 {
            RespWriter::new(&mut reply, proto)
                .error("ERR wrong number of arguments for 'ping' command");
        } else {
            pubsub::subscriber_ping(argv.get(1).copied(), proto, &mut reply);
        }
    } else {
        let sub = argv.get(1).copied();
        pubsub::restricted_error(meta.id, meta.name, sub, &mut RespWriter::new(&mut reply, proto));
    }
    reply
}

fn unavailable_default_reply(proto: Protocol) -> Vec<u8> {
    let mut reply = Vec::new();
    RespWriter::new(&mut reply, proto)
        .error("ERR configured default namespace is unavailable; use SELECT or INF.NS USE");
    reply
}

/// The pump's local mirror arm: conn-state commands execute under a cx
/// mirroring the live connection with the negotiated state written back
/// (HELLO's proto switch must land on the conn — the M0 temp-cx bug);
/// everything else executes locally. Shared verbatim by [`dispatch_one`]
/// and the de-async fast path (ADR-0030 D4). Returns `false` when the
/// connection is gone.
#[allow(clippy::too_many_arguments)] // internal dispatch funnel
fn dispatch_mirror<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    key: ConnKey,
    owned: &OwnedCmd,
    argv: &[&[u8]],
    origin: ExecOrigin,
    proto: Protocol,
    id: u64,
    db: u16,
    conn_ns: Option<NsId>,
    pending: &mut VecDeque<PendingReply>,
) -> bool {
    let mut reply = shared.take_reply_buf();
    if is_conn_state(owned) {
        // Execute under a cx mirroring the live connection, then
        // write the negotiated protocol back — the M0 pump dropped
        // HELLO's proto switch on queued pipelines (temp-cx bug,
        // found extending the surface; ledger entry).
        let Some(mut live) = shared.with_conn(key, |c| ConnCx {
            proto: c.cx.proto,
            id: c.cx.id,
            db: c.cx.db,
            ns: c.cx.ns,
            // Cold path: HELLO/SELECT execute under the live
            // subscription view (the RESP2 subscriber restriction
            // applies to HELLO exactly as in Redis).
            sub_channels: c.cx.sub_channels.clone(),
            sub_patterns: c.cx.sub_patterns.clone(),
            node: Rc::clone(&shared.node),
            close_requested: Cell::new(false),
            program: false,
        }) else {
            return false;
        };
        let now = shared.now.get();
        // The scope a conn-state command executed *under* — SELECT / USE
        // change it for every later command, not for themselves.
        let scope = ExecScope::of(&live);
        execute_slices(argv, &mut shared.store.borrow_mut(), &mut live, now, &mut reply);
        shared.observer.borrow_mut().on_execute(shared.cell, origin, scope, argv, &reply, now);
        shared.with_conn(key, |c| {
            c.cx.proto = live.proto;
            c.cx.db = live.db;
            c.cx.ns = live.ns;
        });
    } else {
        let close =
            shared.execute_owned_into(origin, argv, proto, id, db, conn_ns, false, &mut reply);
        if let Some(dur) = stall_request(argv) {
            shared.stall_until.set(shared.now.get().saturating_add(dur));
        }
        if close {
            close_after_reply(shared, key);
        }
    }
    pending.push_back(PendingReply::Done(reply));
    true
}

/// `QUIT` through the pump (review of 2026-08-30, F-L13-08): the
/// connection closes once the replies ahead of it have flushed, and
/// nothing queued behind it runs — the inline path's `break`, for the
/// pump. Later input is dropped at the read (`close_after_flush`).
pub(super) fn close_after_reply<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Shared<O, F>,
    key: ConnKey,
) {
    shared.with_conn(key, |conn| {
        conn.close_after_flush = true;
        for cmd in conn.queue.drain(..) {
            shared.recycle_cmd_buf(cmd.into_buf());
        }
    });
}

/// Render an owner's outcome as the RESP reply for a whole-argv `Apply`
/// (buffers come from and return to the cell's reply pool).
pub(super) fn render_outcome<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Shared<O, F>,
    outcome: OwnedOutcome,
    proto: Protocol,
) -> Vec<u8> {
    match outcome {
        OwnedOutcome::Bytes(reply) => reply,
        OwnedOutcome::Err(_) => {
            let mut reply = shared.take_reply_buf();
            RespWriter::new(&mut reply, proto).error("ERR cross-cell execution failed");
            reply
        }
        other => {
            // Defensive: typed outcomes from a future peer.
            let mut reply = shared.take_reply_buf();
            let mut w = RespWriter::new(&mut reply, proto);
            match other {
                OwnedOutcome::Ok => w.simple("OK"),
                OwnedOutcome::Int(i) => w.int(i),
                OwnedOutcome::Nil => w.null(),
                OwnedOutcome::Bool(b) => w.bool(b),
                OwnedOutcome::Bytes(_) | OwnedOutcome::Err(_) => {
                    unreachable!("the outer match took Bytes and Err")
                }
            }
            reply
        }
    }
}
