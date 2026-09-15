//! FABRIC-IN: draining peer ops into local execution — one-level batch
//! flattening (F-L18-06: a nested batch is dropped whole, never recursed),
//! apply staging with prefetch, and the parse stage for batched argv.

use super::*;

/// One drained fabric op, handled while its payload still borrows the ring
/// slot (zero copies in): `Reply` completes the origin-side gate inline;
/// `Apply`/`Read` execute against the store and stage their reply bytes
/// into `scratch` (the fabric itself is borrowed by the drain — replies
/// ship right after it ends). `orphans` counts gate-less replies for the
/// fabric tripwire.
///
/// `Batch` is flattened exactly one level here and the leaf handler never
/// recurses (F-L18-06, INFINITY_STYLE §Control flow): the bound is the
/// plane's own, not only the codec's `CodecError::NestedBatch`.
#[allow(clippy::too_many_arguments)] // the FABRIC-IN drain context, not an API surface
pub(super) fn handle_fabric_op<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Shared<O, F>,
    now: Nanos,
    from: CellId,
    op: Op<'_>,
    scratch: &mut Vec<u8>,
    staged: &mut Vec<(CellId, FabricToken, StagedReply)>,
    pubs: &mut Vec<OwnerPub>,
    gated: &mut Vec<GatedReply>,
    orphans: &mut u64,
) {
    match op {
        Op::Batch { ops } => {
            for leaf in ops {
                handle_fabric_leaf(shared, now, from, leaf, scratch, staged, pubs, gated, orphans);
            }
        }
        leaf => handle_fabric_leaf(shared, now, from, leaf, scratch, staged, pubs, gated, orphans),
    }
}

/// Drops a batch found inside a batch without recursion: an explicit work
/// list unwinds the tree and every leaf op is counted on
/// `nested_batch_ops_dropped`. Never reached from the wire (the codec
/// refuses the shape); an in-process producer that builds one loses its
/// ops here instead of the plane's stack.
fn drop_nested_batch<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Shared<O, F>,
    ops: Vec<Op<'_>>,
) {
    let mut pending = ops;
    let mut dropped = 0u64;
    while let Some(op) = pending.pop() {
        match op {
            Op::Batch { ops } => pending.extend(ops),
            _ => dropped += 1,
        }
    }
    shared.nested_batch_ops_dropped.set(shared.nested_batch_ops_dropped.get() + dropped);
}

/// The non-recursive body of [`handle_fabric_op`]: one leaf op.
#[allow(clippy::too_many_arguments)] // the FABRIC-IN drain context, not an API surface
fn handle_fabric_leaf<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Shared<O, F>,
    now: Nanos,
    from: CellId,
    op: Op<'_>,
    scratch: &mut Vec<u8>,
    staged: &mut Vec<(CellId, FabricToken, StagedReply)>,
    pubs: &mut Vec<OwnerPub>,
    gated: &mut Vec<GatedReply>,
    orphans: &mut u64,
) {
    match op {
        Op::Reply { token, outcome } => {
            // Delivery-time hop RTT: inline-handled ops reply in send order
            // per cell pair, so the front send-time entry is this reply's
            // (recording at the pump's await would charge head-of-line
            // parking to the fabric). Pump-deferred replies (`INF.PUB`,
            // M1-S10 — the owner answers after its fan-out acks) interleave
            // arbitrarily; their sends are never recorded, and the token
            // match lets them pass without mispairing the queue.
            {
                let mut sent = shared.rtt_sent.borrow_mut();
                let queue = &mut sent[usize::from(from.0)];
                if queue.front().is_some_and(|&(sent_token, _)| sent_token == token.0) {
                    let (_, t0) = queue.pop_front().expect("front exists");
                    shared.rtt_ns.borrow_mut().record(now.saturating_sub(t0).0);
                }
            }
            // The drained reply already returned one data credit; wake one
            // sender blocked on that destination.
            shared.credit_waiters.wake_one(from);
            // Bytes outcomes own their parked value via the reply pool —
            // no per-reply heap traffic on the steady-state path.
            let owned = match &outcome {
                Outcome::Bytes(bytes) => {
                    let mut buf = shared.take_reply_buf();
                    buf.extend_from_slice(bytes);
                    OwnedOutcome::Bytes(buf)
                }
                other => OwnedOutcome::own(other),
            };
            if !shared.gate.complete(token.0, owned) {
                *orphans += 1;
            }
        }
        Op::Apply { token, cmd, args, program, .. } => {
            handle_apply(shared, from, token, cmd, program, args.as_slice(), scratch, staged, pubs);
        }
        Op::Read { token, key, .. } => {
            let start = scratch.len();
            // M0 vocabulary: the typed Read has no db field; it serves db 0
            // (the M1 paths ship GETs as Apply with the packed db byte).
            let hit = match shared.store.borrow_mut().db_mut(0).get(key, now) {
                Some(value) => {
                    scratch.extend_from_slice(value);
                    true
                }
                None => false,
            };
            let reply =
                if hit { StagedReply::Bytes(start, scratch.len()) } else { StagedReply::Nil };
            staged.push((from, token, reply));
        }
        Op::ApplyNs { token, cmd, ns, args, program, .. } => {
            // Named-namespace apply (M2-S08, ADR-0015 D1): the namespace
            // travels as an explicit id; the owner resolves class and
            // semantics authoritatively (never trusting the origin).
            let argv = args.as_slice();
            let proto = if cmd & 0x0F == 3 { Protocol::Resp3 } else { Protocol::Resp2 };
            // ADR-0115: an internal row on an unmarked frame is unknown —
            // decided here, before the pump can park it.
            if !program && lookup(argv[0]).is_some_and(|m| m.flags.contains(CmdFlags::INTERNAL)) {
                let start = scratch.len();
                crate::exec::unknown_command_reply(argv, proto, scratch);
                staged.push((from, token, StagedReply::Bytes(start, scratch.len())));
                return;
            }
            // A tiered apply can suspend on a cold read — it always
            // defers to the origin's FIFO pump instead of the synchronous
            // drain (M4-S26). A flat *durable*-namespace apply joins the
            // same pump whenever the pump already holds (or is applying)
            // this origin's work — FIFO is the apply-order currency, so
            // nothing may overtake a parked apply (M4.5-S27, ADR-0083
            // D1). Memory namespaces never queue behind durable pressure
            // (namespace isolation). `fabric_in` wakes the pump after
            // this batch.
            let divert = {
                let store = shared.store.borrow();
                store.is_tiered(NsId(ns))
                    || (store.ns_fsync_class(NsId(ns)).is_some()
                        && (shared.ns_pump_active.borrow()[usize::from(from.0)]
                            || !shared.ns_applies.borrow()[usize::from(from.0)].is_empty()))
            };
            if divert {
                shared.ns_applies.borrow_mut()[usize::from(from.0)].push_back(NsApply {
                    token,
                    ns: NsId(ns),
                    proto,
                    args: argv.iter().map(|a| a.to_vec()).collect(),
                    program,
                });
                return;
            }
            // A scattered `DBSIZE` leg on a memory namespace (the tiered
            // and pressured shapes were diverted above): a typed count.
            if argv.len() == 1
                && argv[0].eq_ignore_ascii_case(b"DBSIZE")
                && let Some(n) = {
                    let store = shared.store.borrow();
                    store
                        .ns_get_by_id(NsId(ns))
                        .map(|_| store.ns_store(NsId(ns)).map_or(0, |s| s.len() as i64))
                }
            {
                let now = shared.now.get();
                let mut reply = Vec::new();
                RespWriter::new(&mut reply, Protocol::Resp2).int(n);
                shared.observer.borrow_mut().on_execute(
                    shared.cell,
                    ExecOrigin::Fabric(from),
                    ExecScope::Ns(NsId(ns)),
                    &[b"DBSIZE"],
                    &reply,
                    now,
                );
                staged.push((from, token, StagedReply::Int(n)));
                return;
            }
            let start = scratch.len();
            match shared.execute_ns_owned(from, argv, proto, NsId(ns), program, scratch) {
                NsApplyOutcome::Reply => {
                    staged.push((from, token, StagedReply::Bytes(start, scratch.len())));
                }
                NsApplyOutcome::Gated(seq) => {
                    let reply = scratch[start..].to_vec();
                    scratch.truncate(start);
                    gated.push(GatedReply { to: from, token, seq, reply });
                }
                // Staging pressure: nothing executed or staged — the
                // apply parks on the pump and paces instead of refusing
                // with `-BUSY` (M4.5-S27, ADR-0083 D1).
                NsApplyOutcome::Park => {
                    scratch.truncate(start);
                    shared.ns_applies.borrow_mut()[usize::from(from.0)].push_back(NsApply {
                        token,
                        ns: NsId(ns),
                        proto,
                        args: argv.iter().map(|a| a.to_vec()).collect(),
                        program,
                    });
                }
            }
        }
        // A batch inside a batch: the plane's own bound (F-L18-06).
        Op::Batch { ops } => drop_nested_batch(shared, ops),
        // The M0 plane speaks Apply; a typed Write from a future peer gets
        // a typed refusal rather than silence.
        Op::Write { token, .. } => staged.push((from, token, StagedReply::Refused)),
    }
}

/// The `Op::Apply` body of [`handle_fabric_op`], callable per staged entry
/// by the fabric-apply prefetch batch (M2.5 Phase H): argv may borrow the
/// ring slot (inline path) or the stage scratch (batched path).
#[allow(clippy::too_many_arguments)] // the FABRIC-IN drain context, not an API surface
fn handle_apply<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Shared<O, F>,
    from: CellId,
    token: FabricToken,
    cmd: u8,
    program: bool,
    argv: &[&[u8]],
    scratch: &mut Vec<u8>,
    staged: &mut Vec<(CellId, FabricToken, StagedReply)>,
    pubs: &mut Vec<OwnerPub>,
) {
    // The codec admits a zero-argument apply (`ApplyArgs::EMPTY`); no origin
    // encodes one, since every apply carries a parsed argv. Refuse it typed
    // rather than index an empty argv (L12 style row, review 2026-08-30).
    if argv.is_empty() {
        staged.push((from, token, StagedReply::Refused));
        return;
    }
    {
        {
            // Internal pub/sub fabric vocabulary (M1-S10) — intercepted
            // ahead of `execute`, so it needs no registry entries and stays
            // invisible to clients (an `INF.PUBFAN` typed by a client is an
            // unknown command). One first-byte gate keys the comparisons;
            // only a program-marked frame reaches them (ADR-0115 D5) — an
            // unmarked one falls through to the registry, where it is
            // unknown.
            if program && argv[0].first().is_some_and(|b| b | 0x20 == b'i') {
                if handle_pubsub_apply(shared, from, token, argv, scratch, staged, pubs) {
                    return;
                }
                // Internal namespace-DDL fan (M2-S08): peers apply the
                // origin-assigned spec; invisible to clients (unknown
                // command if typed) — the INF.PUBFAN discipline.
                if handle_ns_apply(shared, from, token, argv, scratch, staged) {
                    return;
                }
            }
            // `cmd` packs `{db:4 | proto:4}` (ADR-0009): the origin
            // connection's SELECTed database rides the existing byte — no
            // codec change; a bare 2/3 from an M0 peer decodes as db 0.
            let proto = if cmd & 0x0F == 3 { Protocol::Resp3 } else { Protocol::Resp2 };
            let db = u16::from(cmd >> 4);
            // Single-key DEL/UNLINK/EXISTS/TOUCH contributions and DBSIZE
            // stay typed for origin-side aggregation; everything else
            // returns the raw RESP reply.
            let counted = argv.len() == 2
                && [&b"DEL"[..], b"UNLINK", b"EXISTS", b"TOUCH"]
                    .iter()
                    .any(|n| argv[0].eq_ignore_ascii_case(n));
            if counted {
                let n = shared.apply_counted(ExecOrigin::Fabric(from), argv[0], argv[1], db);
                staged.push((from, token, StagedReply::Int(n)));
            } else if argv.len() == 1 && argv[0].eq_ignore_ascii_case(b"DBSIZE") {
                let n = shared.apply_dbsize(ExecOrigin::Fabric(from), db);
                staged.push((from, token, StagedReply::Int(n)));
            } else {
                let start = scratch.len();
                shared.execute_owned_into(
                    ExecOrigin::Fabric(from),
                    argv,
                    proto,
                    0,
                    db,
                    None,
                    program,
                    scratch,
                );
                staged.push((from, token, StagedReply::Bytes(start, scratch.len())));
            }
        }
    }
}

/// One fabric `Apply` staged by the prefetch batch (M2.5 Phase H — the
/// ADR-0005 pipeline shape applied to the owner-side fabric path, where a
/// drained pack provides the natural batch the demoted parse-time pipeline
/// never had): argv flat-copied into the stage scratch, key hash computed
/// (and index probe lines prefetched) at stage time.
pub(super) struct StagedApply {
    from: CellId,
    token: FabricToken,
    cmd: u8,
    program: bool,
    /// Offset of the flat argv block in the stage scratch.
    off: u32,
    /// `hasher.hash(argv[1])` when the op carries a key argument.
    hash: u64,
    db: u16,
    has_key: bool,
}

/// Flat-encode `argv` into the stage scratch (the `OwnedCmd` layout:
/// `[argc:u32][end_0..end_{argc-1}:u32][bytes]`, ends relative to the block
/// start). Returns the block offset.
fn stage_argv_block(bytes: &mut Vec<u8>, argv: &[&[u8]]) -> u32 {
    let off = u32::try_from(bytes.len()).expect("stage scratch fits u32");
    let argc = argv.len();
    let head = 4 + 4 * argc;
    bytes.extend_from_slice(&u32::try_from(argc).expect("argc fits u32").to_le_bytes());
    let mut end = head;
    for a in argv {
        end += a.len();
        bytes.extend_from_slice(&u32::try_from(end).expect("block fits u32").to_le_bytes());
    }
    for a in argv {
        bytes.extend_from_slice(a);
    }
    off
}

/// Decode a staged argv block into `out`, returning argc — the inverse of
/// [`stage_argv_block`] (argc ≤ `out.len()` by the stage gates: the parse
/// stage's `PARSE_STAGE_MAX_ARGS`, the fabric stage's
/// [`MAX_INLINE_APPLY_ARGS`]).
fn read_argv_block<'b>(bytes: &'b [u8], off: u32, out: &mut [&'b [u8]]) -> usize {
    let block = &bytes[off as usize..];
    let argc = u32::from_le_bytes(block[..4].try_into().expect("block header")) as usize;
    let mut start = 4 + 4 * argc;
    for (i, slot) in out[..argc].iter_mut().enumerate() {
        let at = 4 + 4 * i;
        let end = u32::from_le_bytes(block[at..at + 4].try_into().expect("ends table")) as usize;
        *slot = &block[start..end];
        start = end;
    }
    argc
}

/// Bounds on what the parse stage accepts (everything has a limit): larger
/// commands act as flush barriers and execute inline — the flat copy of a
/// big SET value would cost more than the misses it hides.
const PARSE_STAGE_MAX_ARGS: usize = 16;
const PARSE_STAGE_MAX_BYTES: usize = 512;

/// Whether the parse loop may stage this fast-path command. Conn-state
/// mutators (HELLO/SELECT/INF.NS), QUIT, and DEBUG are barriers: they
/// mutate `ConnCx`/plane state the inline path handles (close_requested,
/// stall_request), so they keep the inline path and flush the batch first.
/// Unknown commands stay inline (their error reply keeps pipeline order).
pub(super) fn parse_stageable(
    meta: Option<&'static inf_wire::CommandMeta>,
    argv: &ArgvRef<'_>,
) -> bool {
    let Some(meta) = meta else { return false };
    if matches!(
        meta.id,
        CommandId::Hello
            | CommandId::Select
            | CommandId::InfNs
            | CommandId::Quit
            | CommandId::Debug
    ) {
        return false;
    }
    if argv.len() > PARSE_STAGE_MAX_ARGS {
        return false;
    }
    let mut total = 0usize;
    for i in 0..argv.len() {
        total += argv.arg(i).len();
        if total > PARSE_STAGE_MAX_BYTES {
            return false;
        }
    }
    true
}

/// Flat-encode a parsed argv into the stage scratch (the `stage_argv_block`
/// layout over the `Argv` view instead of slices). Returns the block offset.
pub(super) fn stage_argv_block_argv(bytes: &mut Vec<u8>, argv: &ArgvRef<'_>) -> u32 {
    let off = u32::try_from(bytes.len()).expect("stage scratch fits u32");
    let argc = argv.len();
    let head = 4 + 4 * argc;
    bytes.extend_from_slice(&u32::try_from(argc).expect("argc fits u32").to_le_bytes());
    let mut end = head;
    for i in 0..argc {
        end += argv.arg(i).len();
        bytes.extend_from_slice(&u32::try_from(end).expect("block fits u32").to_le_bytes());
    }
    for i in 0..argc {
        bytes.extend_from_slice(argv.arg(i));
    }
    off
}

/// Execute the staged parse batch: one record-line prefetch pass and one
/// document-root pass over the whole batch (§7.3 + ADR-0044 — dependent
/// misses overlap across the batch instead of serializing per command),
/// then execution in parse order through the same
/// `execute` the inline path uses (the `ConnCx` is read live — replies are
/// byte-identical by construction, pinned by the node e2e).
pub(super) fn flush_parse_stage<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Shared<O, F>,
    stage: &mut Vec<StagedParse>,
    stage_bytes: &mut Vec<u8>,
    origin: ExecOrigin,
    conn_cx: &mut ConnCx,
    out: &mut Vec<u8>,
) {
    if stage.is_empty() {
        return;
    }
    {
        let ks = shared.store.borrow();
        if let Some(store) = ks.db(usize::from(conn_cx.db)) {
            for e in stage.iter().filter(|e| e.has_key) {
                store.probe_prefetch(e.hash);
            }
            for e in stage.iter().filter(|e| e.has_key) {
                store.prefetch_doc_root(e.hash);
            }
        }
    }
    let now = shared.now.get();
    let mut argv_buf: [&[u8]; PARSE_STAGE_MAX_ARGS] = [b""; PARSE_STAGE_MAX_ARGS];
    for e in stage.iter() {
        let argc = read_argv_block(stage_bytes, e.off, &mut argv_buf);
        let argv = &argv_buf[..argc];
        let before = out.len();
        // Staged commands are never conn-state, so the scope before and
        // after execution is the same one.
        let scope = ExecScope::of(conn_cx);
        execute(argv, &mut shared.store.borrow_mut(), conn_cx, now, out);
        shared.observer.borrow_mut().on_execute(
            shared.cell,
            origin,
            scope,
            argv,
            &out[before..],
            now,
        );
        // QUIT and DEBUG are stage barriers; a staged command can neither
        // request a close nor a stall.
        debug_assert!(!conn_cx.close_requested.get(), "QUIT is a parse-stage barrier");
    }
    stage.clear();
    stage_bytes.clear();
}

/// The FABRIC-IN drain callback with apply-prefetch on: `Apply` ops stage
/// (copy + hash + index-line prefetch) instead of executing inline; any
/// other op flushes the stage first — an order barrier, so execution and
/// reply order per source pair are exactly the inline path's. `Batch` is
/// flattened exactly one level, never recursed (F-L18-06).
#[allow(clippy::too_many_arguments)] // the FABRIC-IN drain context, not an API surface
pub(super) fn stage_or_handle<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Shared<O, F>,
    now: Nanos,
    from: CellId,
    op: Op<'_>,
    stage: &mut Vec<StagedApply>,
    stage_bytes: &mut Vec<u8>,
    scratch: &mut Vec<u8>,
    staged: &mut Vec<(CellId, FabricToken, StagedReply)>,
    pubs: &mut Vec<OwnerPub>,
    gated: &mut Vec<GatedReply>,
    orphans: &mut u64,
) {
    match op {
        Op::Batch { ops } => {
            for leaf in ops {
                stage_or_handle_leaf(
                    shared,
                    now,
                    from,
                    leaf,
                    stage,
                    stage_bytes,
                    scratch,
                    staged,
                    pubs,
                    gated,
                    orphans,
                );
            }
        }
        leaf => stage_or_handle_leaf(
            shared,
            now,
            from,
            leaf,
            stage,
            stage_bytes,
            scratch,
            staged,
            pubs,
            gated,
            orphans,
        ),
    }
}

/// The non-recursive body of [`stage_or_handle`]: one leaf op.
#[allow(clippy::too_many_arguments)] // the FABRIC-IN drain context, not an API surface
fn stage_or_handle_leaf<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Shared<O, F>,
    now: Nanos,
    from: CellId,
    op: Op<'_>,
    stage: &mut Vec<StagedApply>,
    stage_bytes: &mut Vec<u8>,
    scratch: &mut Vec<u8>,
    staged: &mut Vec<(CellId, FabricToken, StagedReply)>,
    pubs: &mut Vec<OwnerPub>,
    gated: &mut Vec<GatedReply>,
    orphans: &mut u64,
) {
    match op {
        // The stage's argv width is the codec's inline width; a wider apply
        // (ADR-0120 D2 — a spilled argv, rare by construction) runs inline
        // behind the stage like any other order barrier.
        Op::Apply { token, cmd, args, program, .. } if args.len() <= MAX_INLINE_APPLY_ARGS => {
            let argv = args.as_slice();
            let db = u16::from(cmd >> 4);
            let (hash, has_key) = match argv.get(1) {
                Some(key) => {
                    let hash = shared.hasher.hash(key);
                    // Phase 1 at stage time: the index probe lines get the
                    // rest of the drain window to arrive.
                    if let Some(store) = shared.store.borrow().db(usize::from(db)) {
                        store.prefetch(hash);
                    }
                    (hash, true)
                }
                None => (0, false),
            };
            let off = stage_argv_block(stage_bytes, argv);
            stage.push(StagedApply { from, token, cmd, program, off, hash, db, has_key });
        }
        // A batch inside a batch: nothing executes, so no order barrier —
        // dropped whole and counted (F-L18-06).
        Op::Batch { ops } => drop_nested_batch(shared, ops),
        other => {
            flush_apply_stage(shared, stage, stage_bytes, scratch, staged, pubs);
            handle_fabric_leaf(shared, now, from, other, scratch, staged, pubs, gated, orphans);
        }
    }
}

/// Execute the staged applies: one record-line pass and one document-root
/// pass over the whole batch (§7.3 + ADR-0044 — dependent misses overlap
/// across the batch instead of serializing per op), then execution in
/// arrival order.
pub(super) fn flush_apply_stage<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Shared<O, F>,
    stage: &mut Vec<StagedApply>,
    stage_bytes: &mut Vec<u8>,
    scratch: &mut Vec<u8>,
    staged: &mut Vec<(CellId, FabricToken, StagedReply)>,
    pubs: &mut Vec<OwnerPub>,
) {
    if stage.is_empty() {
        return;
    }
    {
        let ks = shared.store.borrow();
        for e in stage.iter().filter(|e| e.has_key) {
            if let Some(store) = ks.db(usize::from(e.db)) {
                store.probe_prefetch(e.hash);
            }
        }
        for e in stage.iter().filter(|e| e.has_key) {
            if let Some(store) = ks.db(usize::from(e.db)) {
                store.prefetch_doc_root(e.hash);
            }
        }
    }
    let mut argv_buf: [&[u8]; MAX_INLINE_APPLY_ARGS] = [b""; MAX_INLINE_APPLY_ARGS];
    for e in stage.iter() {
        let argc = read_argv_block(stage_bytes, e.off, &mut argv_buf);
        handle_apply(
            shared,
            e.from,
            e.token,
            e.cmd,
            e.program,
            &argv_buf[..argc],
            scratch,
            staged,
            pubs,
        );
    }
    stage.clear();
    stage_bytes.clear();
}

/// One MGET position: rendered locally at dispatch, or one remote `GET`.
pub(super) enum GatherPart {
    Done(Vec<u8>),
    Wait(GateWait<u64, OwnedOutcome>),
}

/// Strip the `*1\r\n` header off a single-key `JSON.MGET` sub-reply,
/// leaving the bare element (M3-S11 gather; ADR-0041 D9). Error replies
/// (fabric refusals) pass through untouched — an error is a legal RESP
/// array element.
pub(super) fn strip_single_element(bytes: &[u8]) -> &[u8] {
    match bytes.strip_prefix(b"*1\r\n") {
        Some(element) => element,
        None => {
            debug_assert_eq!(bytes.first(), Some(&b'-'), "sub-replies are *1 arrays or errors");
            bytes
        }
    }
}

#[cfg(test)]
mod fabric_batch_depth {
    use std::rc::Rc;

    use inf_fabric::{FabricToken, Mesh, MeshConfig, Op};
    use inf_foundation::KeySlot;
    use inf_store::{Keyspace, StoreConfig};

    use super::{
        CellId, Nanos, NodeInfo, NoopObserver, ServerPlane, handle_fabric_op, stage_or_handle,
    };

    /// F-L18-06 (review of 2026-08-30): the data plane never recurses on
    /// `Op::Batch`. The codec refuses a nested batch on the wire
    /// (`CodecError::NestedBatch`); this proves the plane holds the bound
    /// on its own — an in-process nested batch (a future sim backend, a
    /// batching optimisation) is dropped whole and counted, on both the
    /// inline and the apply-prefetch drain paths. Pre-fix: each level of
    /// nesting was one more `handle_fabric_op` frame — 100 000 deep
    /// overflowed the stack.
    #[test]
    fn a_nested_batch_is_dropped_whole_never_recursed() {
        const DEPTH: usize = 100_000;
        let mut fabrics = Mesh::new(2, MeshConfig::default());
        let fabric = fabrics.remove(0);
        let plane = ServerPlane::<_, crate::StdSegmentFs>::new(
            CellId(0),
            2,
            -1,
            Keyspace::new(StoreConfig::default()),
            fabric,
            Rc::new(NodeInfo::default()),
            NoopObserver,
            false,
        );
        let shared = &plane.shared;
        let key: &[u8] = b"k";
        let nested = |depth: usize| {
            let mut op = Op::Read { token: FabricToken(7), slot: KeySlot::of_key(key), key };
            for _ in 0..depth {
                op = Op::Batch { ops: vec![op] };
            }
            op
        };
        for prefetch in [false, true] {
            let (mut scratch, mut staged, mut pubs, mut gated, mut orphans) =
                (Vec::new(), Vec::new(), Vec::new(), Vec::new(), 0u64);
            let (mut stage, mut stage_bytes) = (Vec::new(), Vec::new());
            let op = nested(DEPTH);
            if prefetch {
                stage_or_handle(
                    shared,
                    Nanos(0),
                    CellId(1),
                    op,
                    &mut stage,
                    &mut stage_bytes,
                    &mut scratch,
                    &mut staged,
                    &mut pubs,
                    &mut gated,
                    &mut orphans,
                );
            } else {
                handle_fabric_op(
                    shared,
                    Nanos(0),
                    CellId(1),
                    op,
                    &mut scratch,
                    &mut staged,
                    &mut pubs,
                    &mut gated,
                    &mut orphans,
                );
            }
            assert!(staged.is_empty() && stage.is_empty(), "prefetch={prefetch}: {}", staged.len());
            let dropped = shared.nested_batch_ops_dropped.get();
            assert_eq!(dropped, if prefetch { 2 } else { 1 }, "one leaf per nested tree");
            // The legal shape still executes: one level, one leaf, one reply.
            let op = nested(1);
            if prefetch {
                stage_or_handle(
                    shared,
                    Nanos(0),
                    CellId(1),
                    op,
                    &mut stage,
                    &mut stage_bytes,
                    &mut scratch,
                    &mut staged,
                    &mut pubs,
                    &mut gated,
                    &mut orphans,
                );
                super::flush_apply_stage(
                    shared,
                    &mut stage,
                    &mut stage_bytes,
                    &mut scratch,
                    &mut staged,
                    &mut pubs,
                );
            } else {
                handle_fabric_op(
                    shared,
                    Nanos(0),
                    CellId(1),
                    op,
                    &mut scratch,
                    &mut staged,
                    &mut pubs,
                    &mut gated,
                    &mut orphans,
                );
            }
            assert_eq!(staged.len(), 1, "prefetch={prefetch}");
        }
    }
}
