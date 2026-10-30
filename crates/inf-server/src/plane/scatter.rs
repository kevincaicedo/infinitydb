//! Fabric programs (M1-S02): scatter/gather over every cell — `DBSIZE`,
//! `KEYS`, `SCAN`, `RANDOMKEY`, `MSETNX`, the cross-cell `MOVE` (ADR-0110)
//! — and the typed reply parsers the folds and `exec` share.

use super::*;

// ---- fabric-program helpers (M1-S02) -------------------------------------------

/// The fold behind a counted scatter (`DBSIZE` across cells, the
/// `DEL`/`EXISTS`/`TOUCH` families): integers sum; the first error is
/// the reply; **any leg that is neither** — a typed fabric error, or a
/// shape no counted apply produces — is an internal error, so a release
/// build can never answer a partial sum (the review of `44527f4`:
/// the shape was a `debug_assert!` before). A leg's reply is an error by
/// its outcome, never by its bytes (ADR-0190 D2). Pure, so the rows below
/// pin it without a fabric.
pub(super) struct CountedFold {
    acc: i64,
    error: Option<OwnedReply>,
}

impl CountedFold {
    pub(super) fn new(acc: i64, refusal: Option<OwnedReply>) -> CountedFold {
        CountedFold { acc, error: refusal }
    }

    /// Fold one leg. Returns a reply the fold did not keep (an error
    /// after the first), for the caller to recycle.
    pub(super) fn leg(&mut self, outcome: OwnedOutcome, proto: Protocol) -> Option<OwnedReply> {
        let error = match outcome {
            OwnedOutcome::Int(n) => {
                self.acc = self.acc.saturating_add(n);
                return None;
            }
            // A typed error reply from a leg (a tiered `DBSIZE` drain
            // that could not read a twin, ADR-0093 A3).
            OwnedOutcome::Reply(reply) if reply.is_error() => reply,
            OwnedOutcome::Err(_) => owned_error(Vec::new(), proto, CROSS_CELL_EXECUTION_FAILED),
            other @ (OwnedOutcome::Ok
            | OwnedOutcome::Reply(_)
            | OwnedOutcome::Nil
            | OwnedOutcome::Bool(_)) => {
                let text = format!(
                    "ERR internal: a counted leg returned {} instead of a count (fail-closed)",
                    match other {
                        OwnedOutcome::Ok => "OK",
                        OwnedOutcome::Nil => "nil",
                        OwnedOutcome::Bool(_) => "a boolean",
                        OwnedOutcome::Reply(_) => "a non-error reply",
                        OwnedOutcome::Int(_) | OwnedOutcome::Err(_) => {
                            unreachable!("the outer match took Int and Err")
                        }
                    }
                );
                owned_error(Vec::new(), proto, &text)
            }
        };
        if self.error.is_none() {
            self.error = Some(error);
            None
        } else {
            Some(error)
        }
    }

    /// The reply: the sum, or the first error seen.
    pub(super) fn finish(self) -> Result<i64, OwnedReply> {
        match self.error {
            Some(error) => Err(error),
            None => Ok(self.acc),
        }
    }
}

/// The one text of a cross-cell refusal the client reads: a fabric
/// `Err(code)`, a zero-byte leg, or a leg of a shape its fold cannot take
/// (ADR-0190 D2). One `const`, read by every writer of it.
pub(super) const CROSS_CELL_EXECUTION_FAILED: &str = "ERR cross-cell execution failed";

/// An error reply written into `buf` (cleared first), closed with its
/// outcome.
fn owned_error(buf: Vec<u8>, proto: Protocol, text: &str) -> OwnedReply {
    let mut reply = OpenReply::new(buf);
    RespWriter::new(&mut reply, proto).error(text);
    reply.finish()
}

/// All cells except this one (scatter targets).
/// This cell's exact `DBSIZE` contribution for a named namespace: the
/// tiered drain (`tiered::dbsize_count`, ADR-0093 A3) or the memory
/// table's count — the local leg of the scattered shape and every
/// `ApplyNs` leg alike. Reported to the observer per cell, as the
/// default database's `apply_dbsize` is. `Err` is the typed error reply.
pub(super) async fn ns_dbsize_local<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    origin: ExecOrigin,
    ns: NsId,
    proto: Protocol,
) -> Result<i64, OwnedReply> {
    let tiered = shared.store.borrow().is_tiered(ns);
    let n = if tiered {
        tiered::dbsize_count(shared, ns, proto).await? as i64
    } else {
        let ks = shared.store.borrow();
        if ks.ns_get_by_id(ns).is_none() {
            return Err(error_reply(
                shared,
                proto,
                "ERR the selected namespace was dropped (INF.NS USE again)",
            ));
        }
        // A registered namespace whose per-cell store has not
        // materialized yet (no write reached this cell) holds no keys.
        ks.ns_store(ns).map_or(0, |store| store.len() as i64)
    };
    let now = shared.now.get();
    let mut reply = OpenReply::new(Vec::new());
    RespWriter::new(&mut reply, Protocol::Resp2).int(n);
    shared.observer.borrow_mut().on_execute(
        shared.cell,
        origin,
        ExecScope::Ns(ns),
        &[b"DBSIZE"],
        reply.bytes(),
        now,
    );
    Ok(n)
}

pub(super) fn peer_cells<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Shared<O, F>,
) -> Vec<CellId> {
    (0..shared.cells).map(CellId).filter(|c| c.0 != shared.cell.0).collect()
}

pub(super) fn error_reply<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Shared<O, F>,
    proto: Protocol,
    text: &str,
) -> OwnedReply {
    owned_error(shared.take_reply_buf(), proto, text)
}

pub(super) fn int_reply<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Shared<O, F>,
    proto: Protocol,
    n: i64,
) -> OwnedReply {
    let mut reply = OpenReply::new(shared.take_reply_buf());
    RespWriter::new(&mut reply, proto).int(n);
    reply.finish()
}

pub(super) fn simple_reply<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Shared<O, F>,
    proto: Protocol,
    text: &str,
) -> OwnedReply {
    let mut reply = OpenReply::new(shared.take_reply_buf());
    RespWriter::new(&mut reply, proto).simple(text);
    reply.finish()
}

pub(super) fn run_local<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Shared<O, F>,
    origin: ExecOrigin,
    proto: Protocol,
    id: u64,
    db: u16,
    argv: &[&[u8]],
) -> OwnedReply {
    let mut reply = OpenReply::new(shared.take_reply_buf());
    // A program's local leg (ADR-0115 D2).
    reply.lend(|out| shared.execute_owned_into(origin, argv, proto, id, db, None, true, out));
    reply.finish()
}

/// One program step: execute `argv` on `cell` (locally or via Apply) and
/// return its reply, the outcome beside the bytes.
async fn run_on<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    origin: ExecOrigin,
    cell: CellId,
    proto: Protocol,
    id: u64,
    db: u16,
    argv: &[&[u8]],
) -> OwnedReply {
    if cell.0 == shared.cell.0 {
        return run_local(shared, origin, proto, id, db, argv);
    }
    match send_apply(shared, cell, ApplyOrigin::Program, proto, db, argv).await {
        Ok(waiter) => render_outcome(shared, waiter.await, proto),
        Err(refusal) => refusal,
    }
}

/// Write commands that can apply a prefix of their keys and still reply
/// an error (review of 2026-08-30, H2 / F-L17-11, ADR-0098): their
/// effects stage even on an error reply. `stage_durable_effects` reads
/// post-images from the store, so it records exactly what was applied;
/// keys the loop never reached stage their unchanged pre-state
/// (redundant, never wrong). With bounds pre-validated in `exec`, the
/// only remaining trigger is arena OOM mid-way (and the
/// `mset_midway_oom` fault point standing in for it). Every other
/// write-class command is all-or-nothing by construction; a new
/// multi-key write must either validate whole up front or join this
/// set — the durable gate's soundness depends on it.
pub(super) fn stages_despite_error(id: CommandId) -> bool {
    matches!(id, CommandId::Mset | CommandId::Msetnx)
}

/// Whether a write stages its durable effects: on a `Success` reply, or on
/// any reply of a command that stages despite an error. The one staging
/// gate for both execution paths (ADR-0190 D2), read from the reply's
/// outcome, never its bytes.
pub(super) fn stages(outcome: ReplyOutcome, id: CommandId) -> bool {
    outcome == ReplyOutcome::Success || stages_despite_error(id)
}

/// Whether a command on a named namespace executes through the ordinary
/// `execute` path rather than the namespace's tiered arm: the
/// keyspace-level commands `execute` owns regardless of the selected
/// namespace (`SELECT`, `FLUSH*`, cross-db `COPY`, `INFO`, `CONFIG`,
/// `INF.NS`, pub/sub) and every command that addresses nothing in the
/// keyspace (`KeyspaceScope::None` — ADR-0108). **One predicate for both
/// dispatch paths**: the connection's own cell (`dispatch_ns`) and the
/// owner side of an `ApplyNs` leg (`ns_apply_pump`) must agree, or the
/// same command answers differently depending on which cell owns its
/// key — the twin lane found `COPY k k` on a tiered namespace answering
/// the M2 `COPY` refusal locally and the string-family refusal remotely.
pub(super) fn keyspace_level(meta: &'static inf_wire::CommandMeta, argv: &[&[u8]]) -> bool {
    matches!(
        meta.id,
        CommandId::Select
            | CommandId::Flushall
            | CommandId::Flushdb
            | CommandId::Copy
            | CommandId::Info
            | CommandId::Config
            | CommandId::InfNs
            | CommandId::Subscribe
            | CommandId::Unsubscribe
            | CommandId::Psubscribe
            | CommandId::Punsubscribe
            | CommandId::Publish
            | CommandId::Pubsub
    ) || inf_wire::keyspace_scope(meta, argv.get(1).copied()) == inf_wire::KeyspaceScope::None
}

/// Which keyspace a scatter program addresses: the connection's numbered
/// database (legs ride `Apply` with the packed db byte) or a named
/// namespace (legs ride `ApplyNs` — the ADR-0015 D1 op, the shape the
/// S37 `DBSIZE` fix established). Review of 2026-08-30 (C1, F-L13-07):
/// namespace-bound connections must reach the **same** node-wide
/// programs the default database uses — before this, `SCAN`/`KEYS`/
/// `RANDOMKEY` served the connection's own cell and reported a complete
/// answer.
#[derive(Copy, Clone)]
pub(super) enum ScatterScope {
    Db,
    Ns(NsId),
}

/// This cell's leg of a namespace-scoped scatter program: the tiered
/// arm for a tiered namespace (its `SCAN` walks the tiered index and
/// its unsupported commands answer their typed errors), the ordinary
/// namespace execution otherwise. Read-only programs only — a gated
/// (durable-write) verdict here is a dispatch bug, answered as a typed
/// error rather than an unfenced ack.
async fn ns_run_local<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    origin: ExecOrigin,
    proto: Protocol,
    id: u64,
    db: u16,
    ns: NsId,
    argv: &[&[u8]],
) -> OwnedReply {
    let (tiered, class) = {
        let ks = shared.store.borrow();
        (ks.is_tiered(ns), ks.ns_fsync_class(ns))
    };
    if tiered {
        let meta = lookup(argv[0]).expect("scatter programs dispatch registered commands");
        match tiered::dispatch_tiered(shared, origin, ns, meta, argv, proto, class).await {
            tiered::TieredReply::Done(reply) => reply,
            tiered::TieredReply::Gated { reply, .. } => {
                debug_assert!(false, "read-only scatter leg staged a durable effect");
                shared.recycle_reply_buf(reply.into_bytes());
                error_reply(shared, proto, CROSS_CELL_EXECUTION_FAILED)
            }
        }
    } else {
        let mut reply = OpenReply::new(shared.take_reply_buf());
        reply.lend(|out| {
            shared.execute_owned_into(origin, argv, proto, id, db, Some(ns), true, out)
        });
        reply.finish()
    }
}

/// Ship one scatter leg to `to` under `scope` and return the reply
/// waiter (`Apply` for the numbered database, `ApplyNs` for a named
/// namespace). `Err` carries the refusal reply.
async fn scatter_send<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    to: CellId,
    proto: Protocol,
    db: u16,
    scope: ScatterScope,
    argv: &[&[u8]],
) -> Result<GateWait<u64, OwnedOutcome>, OwnedReply> {
    match scope {
        ScatterScope::Db => send_apply(shared, to, ApplyOrigin::Client, proto, db, argv).await,
        ScatterScope::Ns(ns) => {
            send_apply_ns(shared, to, ApplyOrigin::Client, proto, ns, argv).await
        }
    }
}

/// One scope-aware program step on `cell` (the [`run_on`] shape).
#[allow(clippy::too_many_arguments)] // internal dispatch funnel
async fn scatter_run_on<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    origin: ExecOrigin,
    cell: CellId,
    proto: Protocol,
    id: u64,
    db: u16,
    scope: ScatterScope,
    argv: &[&[u8]],
) -> OwnedReply {
    match scope {
        ScatterScope::Db => run_on(shared, origin, cell, proto, id, db, argv).await,
        ScatterScope::Ns(ns) => {
            if cell.0 == shared.cell.0 {
                return ns_run_local(shared, origin, proto, id, db, ns, argv).await;
            }
            match send_apply_ns(shared, cell, ApplyOrigin::Program, proto, ns, argv).await {
                Ok(waiter) => render_outcome(shared, waiter.await, proto),
                Err(refusal) => refusal,
            }
        }
    }
}

/// One typed counted step (EXISTS/DEL shape) on `cell`.
async fn count_on<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    origin: ExecOrigin,
    cell: CellId,
    proto: Protocol,
    db: u16,
    name: &[u8],
    key: &[u8],
) -> Result<i64, OwnedReply> {
    if cell.0 == shared.cell.0 {
        return Ok(shared.apply_counted(origin, name, key, db));
    }
    match send_apply(shared, cell, ApplyOrigin::Program, Protocol::Resp2, db, &[name, key]).await {
        Ok(waiter) => match waiter.await {
            OwnedOutcome::Int(n) => Ok(n),
            OwnedOutcome::Reply(reply) => {
                shared.recycle_reply_buf(reply.into_bytes());
                Err(error_reply(shared, proto, CROSS_CELL_EXECUTION_FAILED))
            }
            OwnedOutcome::Ok | OwnedOutcome::Nil | OwnedOutcome::Bool(_) | OwnedOutcome::Err(_) => {
                Err(error_reply(shared, proto, CROSS_CELL_EXECUTION_FAILED))
            }
        },
        Err(refusal) => Err(refusal),
    }
}

/// Cross-cell MSETNX: existence sweep, then the SET fan. Recorded deviation
/// (compat matrix): check-then-set is not atomic across cells until M4
/// transactions; single-cell MSETNX stays exact.
pub(super) async fn program_msetnx<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    origin: ExecOrigin,
    proto: Protocol,
    id: u64,
    db: u16,
    argv: &[&[u8]],
) -> OwnedReply {
    if argv.len().is_multiple_of(2) {
        return error_reply(shared, proto, "ERR wrong number of arguments for 'msetnx' command");
    }
    let mut i = 1;
    while i < argv.len() {
        let owner = shared.router.cell_of(SlotRouter::slot_of(argv[i]));
        match count_on(shared, origin, owner, proto, db, b"EXISTS", argv[i]).await {
            Ok(0) => {}
            Ok(_) => return int_reply(shared, proto, 0),
            Err(error) => return error,
        }
        i += 2;
    }
    let mut i = 1;
    while i < argv.len() {
        let owner = shared.router.cell_of(SlotRouter::slot_of(argv[i]));
        let reply =
            run_on(shared, origin, owner, Protocol::Resp2, id, db, &[b"SET", argv[i], argv[i + 1]])
                .await;
        if reply.is_error() {
            return reply;
        }
        shared.recycle_reply_buf(reply.into_bytes());
        i += 2;
    }
    int_reply(shared, proto, 1)
}

/// Cross-owner moves snapshot first, put second, and conditionally remove
/// the source last (ADR-0110). A refused put cannot destroy the source;
/// failed cleanup may retain a copy. Full cross-cell atomicity belongs to M6.
#[allow(clippy::wildcard_enum_match_arm, reason = "ADR-0143: column scatter")]
pub(super) async fn program_move<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    origin: ExecOrigin,
    proto: Protocol,
    id: u64,
    db: u16,
    cmd: CommandId,
    argv: &[&[u8]],
) -> OwnedReply {
    let (source, target) = (argv[1], argv[2]);
    let target_owner = shared.router.cell_of(SlotRouter::slot_of(target));
    let (target_db, replace) = match move_options(cmd, argv, db) {
        Ok(options) => options,
        Err(error) => return error_reply(shared, proto, error),
    };
    let snapshot = match move_read(shared, origin, id, db, source, cmd).await {
        Ok(snapshot) => snapshot,
        Err(error) => return error,
    };
    let (value, deadline) = match snapshot {
        Some(snapshot) => snapshot,
        None if cmd == CommandId::Copy => return int_reply(shared, proto, 0),
        None => return error_reply(shared, proto, "ERR no such key"),
    };
    if deadline < -1 {
        return error_reply(shared, proto, "ERR cross-cell program reply malformed");
    }
    if cmd == CommandId::Renamenx {
        // Preserve the existing-target no-op even under OOM. SET NX below
        // still owns the condition if another writer arrives after this read.
        match count_on(shared, origin, target_owner, proto, db, b"EXISTS", target).await {
            Ok(0) => {}
            Ok(_) => return int_reply(shared, proto, 0),
            Err(error) => return error,
        }
    }
    let mut deadline_buf = [0u8; 20];
    let expiry = (deadline >= 0).then(|| crate::exec::fmt_u64(&mut deadline_buf, deadline as u64));
    let put = move_put_args(cmd, target, &value, expiry, replace);
    let reply = run_on(shared, origin, target_owner, Protocol::Resp2, id, target_db, &put).await;
    if reply.is_error() {
        return reply;
    }
    let applied = reply.bytes() == b"+OK\r\n";
    let skipped = reply.bytes() == b"$-1\r\n";
    shared.recycle_reply_buf(reply.into_bytes());
    if skipped && !replace {
        return int_reply(shared, proto, 0);
    }
    if !applied {
        return error_reply(shared, proto, "ERR cross-cell program reply malformed");
    }
    if cmd == CommandId::Copy {
        return int_reply(shared, proto, 1);
    }
    if let Err(error) =
        move_remove(shared, origin, id, db, source, &value, expiry.unwrap_or(b"-1")).await
    {
        return error;
    }
    match cmd {
        CommandId::Rename => simple_reply(shared, proto, "OK"),
        _ => int_reply(shared, proto, 1),
    }
}

/// Builds the bounded put leg; the absolute deadline and NX condition
/// travel together so neither relies on a later follow-up command. The
/// renames ride `INF.PUT`, admitted as RENAME/RENAMENX are (no DENYOOM —
/// ADR-0110 third amendment); `COPY` keeps the client-shaped `SET`, whose
/// DENYOOM is COPY's own. Both reply `+OK` / null-on-NX.
fn move_put_args<'a>(
    cmd: CommandId,
    target: &'a [u8],
    value: &'a [u8],
    deadline: Option<&'a [u8]>,
    replace: bool,
) -> Vec<&'a [u8]> {
    let mut put: Vec<&[u8]> = if cmd == CommandId::Copy {
        let mut put: Vec<&[u8]> = vec![b"SET", target, value];
        if let Some(deadline) = deadline {
            put.extend_from_slice(&[b"PXAT", deadline]);
        }
        put
    } else {
        vec![b"INF.PUT", target, value, deadline.unwrap_or(b"-1")]
    };
    if !replace {
        put.push(b"NX");
    }
    put
}

/// The non-destructive read leg owns its snapshot before it can suspend again.
async fn move_read<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    origin: ExecOrigin,
    id: u64,
    db: u16,
    source: &[u8],
    command: CommandId,
) -> Result<Option<(Vec<u8>, i64)>, OwnedReply> {
    let owner = shared.router.cell_of(SlotRouter::slot_of(source));
    let args: &[&[u8]] = if command == CommandId::Copy {
        &[b"INF.PEEK", source, b"ABS"]
    } else {
        &[b"INF.PEEK", source, b"ABS", b"NOSTATS"]
    };
    let raw = run_on(shared, origin, owner, Protocol::Resp2, id, db, args).await;
    if raw.is_error() {
        return Err(raw);
    }
    let snapshot = parse_take_reply(raw.bytes());
    shared.recycle_reply_buf(raw.into_bytes());
    snapshot.ok_or_else(|| {
        error_reply(shared, Protocol::Resp2, "ERR cross-cell program reply malformed")
    })
}

/// Only an exact match at the source authorizes removal after a successful
/// put. A failure leaves the destination alone: another writer may own it.
async fn move_remove<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    origin: ExecOrigin,
    id: u64,
    db: u16,
    source: &[u8],
    value: &[u8],
    deadline_text: &[u8],
) -> Result<(), OwnedReply> {
    let source_owner = shared.router.cell_of(SlotRouter::slot_of(source));
    let reply = run_on(
        shared,
        origin,
        source_owner,
        Protocol::Resp2,
        id,
        db,
        &[b"INF.TAKE", source, b"IF", value, deadline_text],
    )
    .await;
    if reply.is_error() {
        return Err(reply);
    }
    let removed = reply.bytes() == b":1\r\n";
    let changed = reply.bytes() == b":0\r\n";
    shared.recycle_reply_buf(reply.into_bytes());
    if changed {
        return Err(error_reply(
            shared,
            Protocol::Resp2,
            "BUSY source changed during cross-cell move; destination may contain a copy",
        ));
    }
    if !removed {
        return Err(error_reply(shared, Protocol::Resp2, "ERR cross-cell program reply malformed"));
    }
    Ok(())
}

/// COPY's options are bounded by the command frame; moves without options
/// use the current numbered database and their own replacement condition.
fn move_options(cmd: CommandId, argv: &[&[u8]], db: u16) -> Result<(u16, bool), &'static str> {
    if cmd != CommandId::Copy {
        return Ok((db, cmd == CommandId::Rename));
    }
    let mut target_db = db;
    let mut replace = false;
    let mut i = 3;
    while i < argv.len() {
        if argv[i].eq_ignore_ascii_case(b"REPLACE") {
            replace = true;
        } else if argv[i].eq_ignore_ascii_case(b"DB") && i + 1 < argv.len() {
            target_db = match crate::exec::parse_i64(argv[i + 1]) {
                Ok(n) if (0..i64::from(crate::config::DATABASES)).contains(&n) => n as u16,
                Ok(_) => return Err("ERR DB index is out of range"),
                Err(()) => return Err("ERR value is not an integer or out of range"),
            };
            i += 1;
        } else {
            return Err("ERR syntax error");
        }
        i += 1;
    }
    Ok((target_db, replace))
}

/// Scattered KEYS: local sweep + one Apply per peer, arrays merged by
/// header arithmetic (bodies concatenate untouched).
pub(super) async fn program_keys<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    origin: ExecOrigin,
    proto: Protocol,
    id: u64,
    db: u16,
    scope: ScatterScope,
    argv: &[&[u8]],
) -> OwnedReply {
    let local = match scope {
        ScatterScope::Db => run_local(shared, origin, proto, id, db, argv),
        ScatterScope::Ns(ns) => ns_run_local(shared, origin, proto, id, db, ns, argv).await,
    };
    let Some((mut total, local_off)) = parse_array_header(local.bytes()) else {
        return local; // error passthrough
    };
    let mut waiters = Vec::new();
    for cell in peer_cells(shared) {
        match scatter_send(shared, cell, proto, db, scope, argv).await {
            Ok(waiter) => waiters.push(waiter),
            Err(refusal) => return refusal,
        }
    }
    let mut bodies: Vec<(OwnedReply, usize)> = Vec::new();
    for waiter in waiters {
        match waiter.await {
            OwnedOutcome::Reply(reply) => {
                let Some((n, off)) = parse_array_header(reply.bytes()) else {
                    return reply; // peer error passthrough
                };
                total += n;
                bodies.push((reply, off));
            }
            OwnedOutcome::Ok
            | OwnedOutcome::Int(_)
            | OwnedOutcome::Nil
            | OwnedOutcome::Bool(_)
            | OwnedOutcome::Err(_) => {
                return error_reply(shared, proto, CROSS_CELL_EXECUTION_FAILED);
            }
        }
    }
    let mut reply = OpenReply::new(shared.take_reply_buf());
    {
        let mut w = RespWriter::new(&mut reply, proto);
        w.array_header(total);
        w.encoded(&local.bytes()[local_off..]);
        for (body, off) in &bodies {
            w.encoded(&body.bytes()[*off..]);
        }
    }
    shared.recycle_reply_buf(local.into_bytes());
    for (body, _) in bodies {
        shared.recycle_reply_buf(body.into_bytes());
    }
    reply.finish()
}

/// Scattered SCAN: the cursor packs `{cell:16 | per-cell cursor:48}`; one
/// cell serves each call, the cursor hops to the next cell when a cell's
/// local scan wraps.
pub(super) async fn program_scan<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    origin: ExecOrigin,
    proto: Protocol,
    id: u64,
    db: u16,
    scope: ScatterScope,
    argv: &[&[u8]],
) -> OwnedReply {
    let Some(cursor) = crate::exec::parse_cursor(argv[1]) else {
        return error_reply(shared, proto, "ERR invalid cursor");
    };
    let target = (cursor >> SCAN_CELL_SHIFT) as u16;
    if target >= shared.cells {
        return error_reply(shared, proto, "ERR invalid cursor");
    }
    let mut cursor_buf = [0u8; 20];
    let local_cursor = crate::exec::fmt_u64(&mut cursor_buf, cursor & SCAN_LOCAL_MASK);
    let mut sub: Vec<&[u8]> = argv.to_vec();
    sub[1] = local_cursor;
    let raw = scatter_run_on(shared, origin, CellId(target), proto, id, db, scope, &sub).await;
    let Some((inner, rest_at)) = parse_scan_head(raw.bytes()) else {
        return raw; // error passthrough
    };
    let next = if inner != 0 {
        (u64::from(target) << SCAN_CELL_SHIFT) | inner
    } else if target + 1 < shared.cells {
        u64::from(target + 1) << SCAN_CELL_SHIFT
    } else {
        0
    };
    let mut reply = OpenReply::new(shared.take_reply_buf());
    {
        let mut w = RespWriter::new(&mut reply, proto);
        w.array_header(2);
        let mut next_buf = [0u8; 20];
        w.bulk(crate::exec::fmt_u64(&mut next_buf, next));
        w.encoded(&raw.bytes()[rest_at..]);
    }
    shared.recycle_reply_buf(raw.into_bytes());
    reply.finish()
}

/// Scattered RANDOMKEY: two-level random — a random starting cell, then the
/// first non-empty cell in rotation answers (documented deviation).
pub(super) async fn program_randomkey<
    O: PlaneObserver + 'static,
    F: SegmentFs + Clone + 'static,
>(
    shared: &Rc<Shared<O, F>>,
    origin: ExecOrigin,
    proto: Protocol,
    id: u64,
    db: u16,
    scope: ScatterScope,
    argv: &[&[u8]],
) -> OwnedReply {
    let start = (crate::exec::next_rand(&shared.node) % u64::from(shared.cells)) as u16;
    for i in 0..shared.cells {
        let cell = CellId((start + i) % shared.cells);
        let raw = scatter_run_on(shared, origin, cell, proto, id, db, scope, argv).await;
        if raw.bytes() != b"$-1\r\n" && raw.bytes() != b"_\r\n" {
            return raw;
        }
        shared.recycle_reply_buf(raw.into_bytes());
    }
    let mut reply = OpenReply::new(shared.take_reply_buf());
    RespWriter::new(&mut reply, proto).null();
    reply.finish()
}

/// Decimal text of `v` in a caller-owned stack buffer (the publisher tag
/// never allocates — ADR-0101 D1).
pub(super) fn u64_decimal(buf: &mut [u8; 20], mut v: u64) -> &[u8] {
    let mut at = buf.len();
    loop {
        at -= 1;
        buf[at] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    &buf[at..]
}

/// The `(conn, seq)` publisher tag a fan leg carries (ADR-0101 D2/D3).
/// Malformed text — impossible from a peer, harmless from anything
/// else — reads as untagged.
pub(super) fn parse_publisher_tag(conn: &[u8], seq: &[u8]) -> Option<(ConnKey, u64)> {
    let conn: u64 = core::str::from_utf8(conn).ok()?.parse().ok()?;
    let seq: u64 = core::str::from_utf8(seq).ok()?.parse().ok()?;
    Some((ConnKey::unpack(conn), seq))
}

/// `*N\r\n` array header → `(N, body offset)`. `None` for errors/nulls.
/// Public for the fuzz target.
#[doc(hidden)]
#[cfg_attr(
    not(test),
    deny(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_possible_wrap,
        clippy::arithmetic_side_effects
    )
)]
pub fn parse_array_header(raw: &[u8]) -> Option<(usize, usize)> {
    let rest = raw.strip_prefix(b"*")?;
    let nl = rest.windows(2).position(|w| w == b"\r\n")?;
    let n: i64 = core::str::from_utf8(&rest[..nl]).ok()?.parse().ok()?;
    // A negative count is a null array: no header to walk. `*` + CRLF = 3.
    Some((usize::try_from(n).ok()?, nl.checked_add(3)?))
}

/// `*2\r\n$N\r\n<cursor>\r\n…` SCAN reply head → `(cursor, keys offset)`.
/// Every bound is checked before it is indexed (F-L12-06): the cursor's
/// trailing CRLF must exist, so the offset never lands past the buffer
/// (`&raw[rest_at..]` at the caller). Public for the fuzz target.
#[doc(hidden)]
#[cfg_attr(
    not(test),
    deny(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_possible_wrap,
        clippy::arithmetic_side_effects
    )
)]
pub fn parse_scan_head(raw: &[u8]) -> Option<(u64, usize)> {
    let rest = raw.strip_prefix(b"*2\r\n$")?;
    let nl = rest.windows(2).position(|w| w == b"\r\n")?;
    let len: usize = core::str::from_utf8(&rest[..nl]).ok()?.parse().ok()?;
    let start = nl.checked_add(2)?;
    let end = start.checked_add(len)?;
    let cursor = crate::exec::parse_cursor(rest.get(start..end)?)?;
    if rest.get(end..end.checked_add(2)?)? != b"\r\n" {
        return None;
    }
    // `*2\r\n$` (5) before `rest`, the cursor's CRLF (2) after `end`.
    Some((cursor, end.checked_add(7)?))
}

/// Two-field snapshot reply: `*-1` means missing; `*2 [$value][:time]`
/// carries milliseconds (-1 means no expiry). Legacy forms return remaining
/// TTL; the move program's `INF.PEEK key ABS` returns absolute Unix expiry.
#[cfg_attr(
    not(test),
    deny(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_possible_wrap,
        clippy::arithmetic_side_effects
    )
)]
pub fn parse_take_reply(raw: &[u8]) -> Option<Option<(Vec<u8>, i64)>> {
    if raw == b"*-1\r\n" {
        return Some(None);
    }
    let rest = raw.strip_prefix(b"*2\r\n$")?;
    let nl = rest.windows(2).position(|w| w == b"\r\n")?;
    let len: usize = core::str::from_utf8(&rest[..nl]).ok()?.parse().ok()?;
    let start = nl.checked_add(2)?;
    let end = start.checked_add(len)?;
    let value = rest.get(start..end)?;
    let tail = rest.get(end..)?.strip_prefix(b"\r\n:")?.strip_suffix(b"\r\n")?;
    let pttl: i64 = core::str::from_utf8(tail).ok()?.parse().ok()?;
    Some(Some((value.to_vec(), pttl)))
}

/// The store-level fold behind [`ServerPlane::fold_live_entries`], shared
/// with the sim's replay model so node and model are walked by one
/// implementation (a walker that differed per side would be the oracle
/// encoding its own bug). Returns the number of tiered namespaces skipped.
pub fn fold_live_entries(
    ks: &mut Keyspace,
    now: Nanos,
    mut emit: impl FnMut(ExecScope, &[u8], &[u8], Option<u64>),
) -> usize {
    let dbs: Vec<usize> = ks.dbs().map(|(i, _)| i).collect();
    for db in dbs {
        let scope = ExecScope::Db(u16::try_from(db).expect("db index validated at SELECT"));
        walk_live_entries(ks.db_mut(db), scope, now, &mut emit);
    }
    let named: Vec<(NsId, bool)> =
        ks.ns_iter().map(|spec| (spec.id, spec.tier.is_some())).collect();
    let mut tiered_skipped = 0;
    for (id, tiered) in named {
        if tiered {
            tiered_skipped += 1;
            continue;
        }
        // A registered namespace no write reached on this cell has no
        // store and no entries; `ns_store_mut` would materialize one.
        if ks.ns_store(id).is_none() {
            continue;
        }
        let store = ks.ns_store_mut(id).expect("materialized, checked above");
        walk_live_entries(store, ExecScope::Ns(id), now, &mut emit);
    }
    tiered_skipped
}

/// One store's live entries through the resize-stable checkpoint walk, run
/// to completion; documents emit their canonical idoc bytes as the value.
fn walk_live_entries(
    store: &mut CellStore,
    scope: ExecScope,
    now: Nanos,
    emit: &mut impl FnMut(ExecScope, &[u8], &[u8], Option<u64>),
) {
    let mut cursor = 0;
    loop {
        cursor =
            store.scan_checkpoint_images(
                cursor,
                usize::MAX,
                now,
                |key, image, deadline| match image {
                    inf_store::CheckpointImage::String(value) => emit(scope, key, value, deadline),
                    #[cfg(feature = "doc")]
                    inf_store::CheckpointImage::JsonDoc { idoc, .. } => {
                        emit(scope, key, idoc, deadline)
                    }
                },
            );
        if cursor == 0 {
            break;
        }
    }
}

/// Owned-slice twin of `extract_keys` (the wire helper wants an `ArgvRef`).
pub(super) fn extract_keys_slices<'a>(
    meta: &inf_wire::CommandMeta,
    argv: &[&'a [u8]],
) -> Vec<&'a [u8]> {
    extract_keys_iter(meta, argv).collect()
}

/// Non-allocating key iterator over owned slices — the dispatch hot path
/// probes key routing once per command without a `Vec` per probe (M2.5
/// Phase H allocator lever). Semantics identical to `inf_wire::extract_keys`
/// (both read `inf_wire::key_spec`, the subcommand-aware routing truth —
/// ADR-0104): `first == 0` or `step == 0` yields nothing; `last < 0`
/// counts from the end; iteration stops at the argv boundary.
pub(super) fn extract_keys_iter<'v, 'a>(
    meta: &inf_wire::CommandMeta,
    argv: &'v [&'a [u8]],
) -> impl Iterator<Item = &'a [u8]> + 'v {
    let spec = inf_wire::key_spec(meta, argv.get(1).copied());
    let last = if spec.last >= 0 {
        spec.last as usize
    } else {
        argv.len().saturating_sub(spec.last.unsigned_abs() as usize)
    };
    let (start, last) = if spec.first == 0 || spec.step == 0 || argv.is_empty() {
        (1, 0) // empty range
    } else {
        (usize::from(spec.first), last)
    };
    (start..=last).step_by(usize::from(spec.step).max(1)).map_while(move |i| argv.get(i).copied())
}

#[cfg(test)]
mod scatter_reply_parsers {
    use super::{parse_array_header, parse_scan_head, parse_take_reply};

    /// F-L12-06: a SCAN head without its trailing CRLF must be refused,
    /// never answered with an offset past the buffer.
    #[test]
    fn scan_head_without_a_terminator_is_refused() {
        assert_eq!(parse_scan_head(b"*2\r\n$1\r\n0\r\n"), Some((0, 11)));
        assert_eq!(parse_scan_head(b"*2\r\n$1\r\n0"), None);
        assert_eq!(parse_scan_head(b"*2\r\n$1\r\n0\r"), None);
        assert_eq!(parse_scan_head(b"*2\r\n$1\r\n0\n\n"), None);
        assert_eq!(parse_scan_head(b"*2\r\n$2\r\n0"), None);
        let ok = b"*2\r\n$3\r\n123\r\n*0\r\n";
        let (cursor, at) = parse_scan_head(ok).expect("well formed");
        assert_eq!((cursor, &ok[at..]), (123, &b"*0\r\n"[..]));
    }

    /// The array count at its edges: zero, one, null (negative), the i64
    /// maximum, and one past it; the offset is always `*` + digits + CRLF.
    #[test]
    fn array_header_count_is_narrowed_never_wrapped() {
        assert_eq!(parse_array_header(b"*0\r\n"), Some((0, 4)));
        assert_eq!(parse_array_header(b"*1\r\nx"), Some((1, 4)));
        assert_eq!(parse_array_header(b"*-1\r\n"), None, "a null array has no body");
        assert_eq!(parse_array_header(b"*-9223372036854775808\r\n"), None);
        assert_eq!(
            parse_array_header(b"*9223372036854775807\r\n"),
            Some((i64::MAX as usize, 22)),
            "the largest count a peer can state"
        );
        assert_eq!(parse_array_header(b"*9223372036854775808\r\n"), None, "past i64");
        assert_eq!(parse_array_header(b"*\r\n"), None, "no digits");
    }

    /// The keys offset is the head's exact length, at the smallest cursor
    /// and at `u64::MAX`.
    #[test]
    fn scan_head_offset_is_the_head_length() {
        for cursor in ["0", "18446744073709551615"] {
            let head = format!("*2\r\n${}\r\n{cursor}\r\n", cursor.len());
            let raw = format!("{head}*0\r\n");
            let parsed = parse_scan_head(raw.as_bytes()).expect("a well-formed head");
            assert_eq!(parsed, (cursor.parse().expect("u64"), head.len()));
        }
        assert_eq!(
            parse_scan_head(b"*2\r\n$18446744073709551615\r\n0\r\n"),
            None,
            "length past the buffer"
        );
    }

    #[test]
    fn every_scatter_parser_is_total_on_short_input() {
        let samples: &[&[u8]] = &[
            b"",
            b"*",
            b"*2",
            b"*2\r\n$",
            b"*2\r\n$1",
            b"*2\r\n$1\r\n",
            b"*2\r\n$1\r\n0",
            b"*2\r\n$99999999999999999999\r\n0\r\n",
            b"*-1\r",
            b"*-2\r\n",
            b"*2\r\n$1\r\nx\r\n:",
        ];
        for raw in samples {
            for n in 0..=raw.len() {
                let _ = parse_scan_head(&raw[..n]);
                let _ = parse_take_reply(&raw[..n]);
                let _ = parse_array_header(&raw[..n]);
            }
        }
    }
}

#[cfg(test)]
mod counted_fold_tests {
    use inf_fabric::ErrCode;

    use super::{CountedFold, OwnedOutcome, OwnedReply, Protocol, ReplyOutcome};

    /// A leg's reply as the decode builds it, its outcome from the wire.
    fn leg_reply(bytes: &[u8], outcome: ReplyOutcome) -> OwnedReply {
        OwnedReply::received(bytes.to_vec(), outcome).expect("a non-empty reply")
    }

    fn error(bytes: &[u8]) -> OwnedReply {
        leg_reply(bytes, ReplyOutcome::Error)
    }

    fn fold(legs: Vec<OwnedOutcome>) -> Result<i64, OwnedReply> {
        let mut fold = CountedFold::new(0, None);
        for leg in legs {
            let _ = fold.leg(leg, Protocol::Resp2);
        }
        fold.finish()
    }

    /// Counts sum; a leg's error reply is the answer.
    #[test]
    fn counts_sum_and_a_leg_error_is_the_reply() {
        assert_eq!(fold(vec![OwnedOutcome::Int(2), OwnedOutcome::Int(3)]), Ok(5));
        assert_eq!(
            fold(vec![
                OwnedOutcome::Int(2),
                OwnedOutcome::Reply(error(b"-ERR twin\r\n")),
                OwnedOutcome::Int(3),
            ]),
            Err(error(b"-ERR twin\r\n"))
        );
    }

    /// The review of `44527f4`: an unexpected leg outcome was a
    /// `debug_assert!`, so a release build summed the rest. Now every
    /// non-count leg is a typed error and no integer is ever returned;
    /// a success reply is foreign whatever its bytes.
    #[test]
    fn an_injected_typed_error_or_foreign_shape_never_yields_a_count() {
        for foreign in [
            OwnedOutcome::Err(ErrCode::OutOfMemory),
            OwnedOutcome::Err(ErrCode::Unknown(7)),
            OwnedOutcome::Ok,
            OwnedOutcome::Nil,
            OwnedOutcome::Bool(true),
            OwnedOutcome::Reply(leg_reply(b"+OK\r\n", ReplyOutcome::Success)),
            OwnedOutcome::Reply(leg_reply(b"-ERR tagged success\r\n", ReplyOutcome::Success)),
        ] {
            let label = format!("{foreign:?}");
            let reply =
                fold(vec![OwnedOutcome::Int(2), foreign, OwnedOutcome::Int(5)]).expect_err(&label);
            assert!(reply.is_error(), "{label}: {reply:?}");
        }
    }

    /// The first error wins; later replies are handed back for recycling,
    /// and a refusal already in hand outranks every leg.
    #[test]
    fn the_first_error_wins_and_later_buffers_are_handed_back() {
        let mut fold = CountedFold::new(0, None);
        assert!(fold.leg(OwnedOutcome::Reply(error(b"-ERR one\r\n")), Protocol::Resp2).is_none());
        assert_eq!(
            fold.leg(OwnedOutcome::Reply(error(b"-ERR two\r\n")), Protocol::Resp2),
            Some(error(b"-ERR two\r\n"))
        );
        assert!(fold.leg(OwnedOutcome::Err(ErrCode::WrongType), Protocol::Resp2).is_some());
        assert_eq!(fold.finish(), Err(error(b"-ERR one\r\n")));
        let mut refused = CountedFold::new(4, Some(error(b"-ERR refused\r\n")));
        assert!(refused.leg(OwnedOutcome::Int(1), Protocol::Resp2).is_none());
        assert_eq!(refused.finish(), Err(error(b"-ERR refused\r\n")));
    }

    /// A fabric refusal renders the one cross-cell refusal text.
    #[test]
    fn a_refused_leg_renders_the_one_refusal_text() {
        let reply = fold(vec![OwnedOutcome::Err(ErrCode::EmptyReply)]).expect_err("refused");
        assert_eq!(reply.bytes(), b"-ERR cross-cell execution failed\r\n");
    }
}
