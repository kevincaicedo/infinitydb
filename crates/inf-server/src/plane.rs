//! `ServerPlane` — the M0 node assembly: one cell's complete data plane,
//! implementing [`CellPlane`] over any backend driver (uring in `infinityd`,
//! kqueue on the dev tier, the sim driver in `inf-sim`).
//!
//! ## Shape
//! - **Connections** live in a slab keyed `{slot:24, gen:32}` — exactly the
//!   completion-token model, so a stale completion can never touch a reused
//!   slot.
//! - **Local commands** (every key owned by this cell, or no keys) execute
//!   synchronously inside PARSE+EXECUTE — the L6 fast path pays nothing.
//! - **Remote commands** run on a per-connection *pump* future executing the
//!   connection's commands strictly in order (pipelined replies must never
//!   reorder), suspending on a [`FabricGate`] for replies and on a
//!   [`WaitList`] when fabric credits are exhausted. While a pump is active,
//!   later commands queue behind it; past a watermark the connection's recv
//!   is disarmed — credit backpressure reaches TCP (master plan §6.1).
//! - **Cross-cell vocabulary** (M0-experimental `Apply`, reshaped by M4):
//!   single-owner commands ship as `Op::Apply { cmd: protocol, args: argv }`
//!   and return the owner's raw RESP reply (`Outcome::Bytes`) — byte-exact
//!   by construction. `DEL`/`EXISTS` (the only multi-key M0 commands) split
//!   per key and aggregate typed `Outcome::Int` replies.
//! - **Observer seam**: every apply point (local execution, and the owner
//!   side of a remote `Apply`) reports `(argv, reply, now)` — `inf-sim`'s
//!   linearizability oracle hangs off this; [`NoopObserver`] monomorphizes
//!   to nothing in `infinityd`.

use core::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;

use inf_alloc::{BufferId, LeaseKind};
use inf_fabric::{ApplyArgs, CellFabric, ErrCode, FabricToken, Op, Outcome, SendError};
use inf_foundation::time::Nanos;
use inf_foundation::{CellId, LogHistogram};
use inf_runtime::GroupClass;
use inf_runtime::{
    CellPlane, Completion, CompletionResult, CompletionToken, FabricGate, IoOp, LoopCx, RawFd,
    TokenClass, WaitList,
};
use inf_store::{CellStore, SlotRouter};
use inf_wire::{
    ArgvRef, CommandId, ConnParser, Parsed, ParserLimits, Protocol, RespWriter, arity_ok,
    extract_keys, lookup,
};

use crate::exec::{ConnCx, NodeInfo, execute, execute_slices};

/// Commands queued behind an active pump before recv is disarmed (bounded
/// everything — the backpressure watermark).
const PENDING_HIGH_WATER: usize = 1024;
/// Re-arm recv once the queue drains to this.
const PENDING_LOW_WATER: usize = 64;
/// Max fabric ops drained per FABRIC-IN step (bounded drain).
const FABRIC_DRAIN_MAX: usize = 1024;

/// Apply-point hook (sim oracle seam).
pub trait PlaneObserver {
    /// One command applied on this cell: `argv` and the RESP reply bytes it
    /// produced, at injected time `now`.
    fn on_execute(
        &mut self,
        cell: CellId,
        origin: ExecOrigin,
        argv: &[&[u8]],
        reply: &[u8],
        now: Nanos,
    );
}

/// Where an applied command came from.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum ExecOrigin {
    /// A connection on this cell (slab slot, generation).
    Conn(u32, u32),
    /// A fabric `Apply` on behalf of the origin cell.
    Fabric(CellId),
}

/// Observer that observes nothing (the production default).
#[derive(Default, Debug)]
pub struct NoopObserver;

impl PlaneObserver for NoopObserver {
    #[inline]
    fn on_execute(&mut self, _: CellId, _: ExecOrigin, _: &[&[u8]], _: &[u8], _: Nanos) {}
}

/// Owned fabric outcome (decoded outcomes borrow ring slots; gate values
/// must own their bytes).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum OwnedOutcome {
    Ok,
    Bytes(Vec<u8>),
    Int(i64),
    Nil,
    Bool(bool),
    Err(ErrCode),
}

impl OwnedOutcome {
    fn own(outcome: &Outcome<'_>) -> OwnedOutcome {
        match outcome {
            Outcome::Ok => OwnedOutcome::Ok,
            Outcome::Bytes(b) => OwnedOutcome::Bytes(b.to_vec()),
            Outcome::Int(i) => OwnedOutcome::Int(*i),
            Outcome::Nil => OwnedOutcome::Nil,
            Outcome::Bool(b) => OwnedOutcome::Bool(*b),
            Outcome::Err(e) => OwnedOutcome::Err(*e),
        }
    }
}

// ---- connection slab ---------------------------------------------------------

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
struct ConnKey {
    slot: u32,
    generation: u32,
}

struct Conn {
    fd: RawFd,
    parser: ConnParser,
    cx: ConnCx,
    /// Staged reply bytes awaiting RESPOND.
    out: Vec<u8>,
    /// One in-flight Send max: two outstanding sends on one socket have no
    /// kernel ordering guarantee.
    send_inflight: bool,
    closing: bool,
    close_after_flush: bool,
    /// A pump future owns this connection's execution order.
    pump_active: bool,
    queue: VecDeque<Vec<Vec<u8>>>,
    recv_disarmed: bool,
    rearm_recv: bool,
}

impl Conn {
    fn state_bytes(&self) -> usize {
        size_of::<Conn>()
            + self.parser.buffered()
            + self.out.capacity()
            + self.queue.iter().map(|c| c.iter().map(Vec::capacity).sum::<usize>()).sum::<usize>()
    }
}

#[derive(Default)]
struct ConnSlab {
    slots: Vec<Option<Conn>>,
    gens: Vec<u32>,
    free: Vec<u32>,
    live: usize,
}

impl ConnSlab {
    fn insert(&mut self, conn: Conn) -> ConnKey {
        self.live += 1;
        if let Some(slot) = self.free.pop() {
            self.slots[slot as usize] = Some(conn);
            return ConnKey { slot, generation: self.gens[slot as usize] };
        }
        let slot = u32::try_from(self.slots.len()).expect("conn slots fit u32");
        assert!(slot < (1 << 24), "conn slot exceeds token slot width");
        self.slots.push(Some(conn));
        self.gens.push(0);
        ConnKey { slot, generation: 0 }
    }

    fn get_mut(&mut self, key: ConnKey) -> Option<&mut Conn> {
        if self.gens.get(key.slot as usize) != Some(&key.generation) {
            return None;
        }
        self.slots.get_mut(key.slot as usize).and_then(Option::as_mut)
    }

    fn remove(&mut self, key: ConnKey) -> Option<Conn> {
        if self.gens.get(key.slot as usize) != Some(&key.generation) {
            return None;
        }
        let conn = self.slots.get_mut(key.slot as usize).and_then(Option::take);
        if conn.is_some() {
            self.gens[key.slot as usize] = self.gens[key.slot as usize].wrapping_add(1);
            self.free.push(key.slot);
            self.live -= 1;
        }
        conn
    }

    fn keys(&self) -> Vec<ConnKey> {
        self.slots
            .iter()
            .enumerate()
            .filter(|(_, c)| c.is_some())
            .map(|(slot, _)| ConnKey { slot: slot as u32, generation: self.gens[slot] })
            .collect()
    }
}

// ---- shared cell state (futures hold an Rc) -----------------------------------

struct Shared<O: PlaneObserver + 'static> {
    cell: CellId,
    router: SlotRouter,
    /// Forces every key local — the cross-cell penalty A/B leg (§6 gate).
    route_local_only: bool,
    store: RefCell<CellStore>,
    fabric: RefCell<CellFabric>,
    conns: RefCell<ConnSlab>,
    gate: FabricGate<OwnedOutcome>,
    credit_waiters: WaitList<CellId>,
    observer: RefCell<O>,
    node: Rc<NodeInfo>,
    /// Loop-granularity clock for futures (set each step from `cx.now`).
    now: Cell<Nanos>,
    /// Fabric token round-trip latency, nanoseconds (hop RTT gate).
    rtt_ns: RefCell<LogHistogram>,
    recv_dropped: Cell<u64>,
}

impl<O: PlaneObserver + 'static> Shared<O> {
    fn with_conn<R>(&self, key: ConnKey, f: impl FnOnce(&mut Conn) -> R) -> Option<R> {
        self.conns.borrow_mut().get_mut(key).map(f)
    }

    /// Executes owned argv locally (queued and remote-`Apply` paths) and
    /// reports the apply point; returns the reply bytes.
    fn execute_owned(
        &self,
        origin: ExecOrigin,
        argv: &[&[u8]],
        proto: Protocol,
        id: u64,
    ) -> Vec<u8> {
        let mut out = Vec::new();
        let mut cx = ConnCx { proto, id, node: Rc::clone(&self.node) };
        let now = self.now.get();
        execute_slices(argv, &mut self.store.borrow_mut(), &mut cx, now, &mut out);
        self.observer.borrow_mut().on_execute(self.cell, origin, argv, &out, now);
        out
    }

    /// Typed single-key DEL/EXISTS apply (local or owner side): the reply is
    /// the integer count contribution; observer sees the synthesized
    /// single-key command with its `:N` reply.
    fn apply_counted(&self, origin: ExecOrigin, del: bool, key: &[u8]) -> i64 {
        let now = self.now.get();
        let hit = {
            let mut store = self.store.borrow_mut();
            if del { store.del(key, now) } else { store.exists(key, now) }
        };
        let mut reply = Vec::new();
        RespWriter::new(&mut reply, Protocol::Resp2).int(i64::from(hit));
        let name: &[u8] = if del { b"DEL" } else { b"EXISTS" };
        self.observer.borrow_mut().on_execute(self.cell, origin, &[name, key], &reply, now);
        i64::from(hit)
    }
}

/// One cell's data plane. Construct per cell, drive with
/// [`CellLoop::run_iteration`](inf_runtime::CellLoop::run_iteration).
pub struct ServerPlane<O: PlaneObserver + 'static = NoopObserver> {
    shared: Rc<Shared<O>>,
    listener: RawFd,
    started: bool,
    /// Recv completions staged from step 1 for PARSE+EXECUTE (step 3+4).
    inbox: Vec<(ConnKey, BufferId, u32)>,
}

impl<O: PlaneObserver + 'static> ServerPlane<O> {
    /// `listener` must be a listening fd this plane's driver will own.
    #[allow(clippy::too_many_arguments)] // construction-time wiring, not an API surface
    pub fn new(
        cell: CellId,
        cells: u16,
        listener: RawFd,
        store: CellStore,
        fabric: CellFabric,
        node: Rc<NodeInfo>,
        observer: O,
        route_local_only: bool,
    ) -> ServerPlane<O> {
        node.cell.set(cell.0);
        node.cells.set(cells);
        ServerPlane {
            shared: Rc::new(Shared {
                cell,
                router: SlotRouter::new_contiguous(cells),
                route_local_only,
                store: RefCell::new(store),
                fabric: RefCell::new(fabric),
                conns: RefCell::new(ConnSlab::default()),
                gate: FabricGate::new(),
                credit_waiters: WaitList::new(),
                observer: RefCell::new(observer),
                node,
                now: Cell::new(Nanos(0)),
                rtt_ns: RefCell::new(LogHistogram::new()),
                recv_dropped: Cell::new(0),
            }),
            listener,
            started: false,
            inbox: Vec::new(),
        }
    }

    /// Live connections (tests, stats).
    pub fn connections(&self) -> usize {
        self.shared.conns.borrow().live
    }

    /// Outstanding async work: pending fabric replies + credit waiters.
    /// Quiescence (sim) means zero.
    pub fn suspended(&self) -> usize {
        self.shared.gate.pending() + self.shared.credit_waiters.waiting()
    }

    fn token(class: TokenClass, key: ConnKey) -> CompletionToken {
        CompletionToken::new(class, key.slot, key.generation)
    }

    fn key_of(token: CompletionToken) -> ConnKey {
        ConnKey { slot: token.slot(), generation: token.generation() }
    }

    /// True when at least one key of a well-formed command is owned by
    /// another cell.
    fn needs_fabric(&self, argv: &ArgvRef<'_>) -> bool {
        if self.shared.route_local_only {
            return false;
        }
        let Some(meta) = lookup(argv.arg(0)) else { return false };
        if !arity_ok(meta, argv.len()) {
            return false;
        }
        extract_keys(meta, argv).any(|key| !self.shared.router.is_local(key, self.shared.cell))
    }

    fn initiate_close(&mut self, cx: &mut LoopCx<'_>, key: ConnKey) {
        if let Some(fd) = self.shared.with_conn(key, |conn| {
            conn.closing = true;
            conn.fd
        }) {
            cx.push(IoOp::Close { fd, token: Self::token(TokenClass::Close, key) });
        }
    }

    /// Spawn the per-connection ordered pump with its first command.
    fn spawn_pump(&self, cx: &mut LoopCx<'_>, key: ConnKey, first: Vec<Vec<u8>>) {
        let shared = Rc::clone(&self.shared);
        let _ = cx.executor.poll_immediate(async move {
            let mut cmd = Some(first);
            while let Some(owned) = cmd.take() {
                run_one(&shared, key, &owned).await;
                cmd = shared
                    .with_conn(key, |conn| {
                        let next = conn.queue.pop_front();
                        if next.is_none() {
                            conn.pump_active = false;
                        }
                        if conn.recv_disarmed && conn.queue.len() <= PENDING_LOW_WATER {
                            conn.rearm_recv = true;
                        }
                        next
                    })
                    .flatten();
            }
        });
    }
}

impl<O: PlaneObserver + 'static> CellPlane for ServerPlane<O> {
    fn on_completion(&mut self, cx: &mut LoopCx<'_>, c: Completion) {
        match c.result {
            CompletionResult::Accepted { fd } => {
                let key = self.shared.conns.borrow_mut().insert(Conn {
                    fd,
                    parser: ConnParser::new(ParserLimits::default()),
                    cx: ConnCx {
                        proto: Protocol::Resp2,
                        id: 0,
                        node: Rc::clone(&self.shared.node),
                    },
                    out: Vec::new(),
                    send_inflight: false,
                    closing: false,
                    close_after_flush: false,
                    pump_active: false,
                    queue: VecDeque::new(),
                    recv_disarmed: false,
                    rearm_recv: false,
                });
                let id = (u64::from(key.slot) << 32) | u64::from(key.generation);
                self.shared.with_conn(key, |conn| conn.cx.id = id);
                cx.push(IoOp::RecvArm { fd, token: Self::token(TokenClass::Recv, key) });
            }
            CompletionResult::Recv { buf, len } => {
                let key = Self::key_of(c.token);
                if len == 0 {
                    cx.pool.release(buf);
                    let live = self.shared.with_conn(key, |conn| !conn.closing).unwrap_or(false);
                    if live {
                        self.initiate_close(cx, key);
                    }
                } else {
                    self.inbox.push((key, buf, len));
                }
            }
            CompletionResult::RecvDropped => {
                self.shared.recv_dropped.set(self.shared.recv_dropped.get() + 1);
            }
            CompletionResult::Sent { buf } => {
                cx.pool.release(buf);
                let key = Self::key_of(c.token);
                self.shared.with_conn(key, |conn| conn.send_inflight = false);
            }
            CompletionResult::Closed => {
                let key = Self::key_of(c.token);
                self.shared.conns.borrow_mut().remove(key);
            }
            CompletionResult::Error { buf, .. } => {
                if let Some(buf) = buf {
                    cx.pool.release(buf);
                }
                let key = Self::key_of(c.token);
                let live = self
                    .shared
                    .with_conn(key, |conn| {
                        conn.send_inflight = false;
                        !conn.closing
                    })
                    .unwrap_or(false);
                if live {
                    self.initiate_close(cx, key);
                }
            }
        }
    }

    fn fabric_in(&mut self, cx: &mut LoopCx<'_>) {
        self.shared.now.set(cx.now);
        enum Staged {
            Apply { token: FabricToken, proto: u8, args: Vec<Vec<u8>> },
            Read { token: FabricToken, key: Vec<u8> },
            Reply { token: FabricToken, outcome: OwnedOutcome },
            Unsupported { token: FabricToken },
        }
        fn stage(staged: &mut Vec<(CellId, Staged)>, from: CellId, op: Op<'_>) {
            match op {
                Op::Apply { token, cmd, args, .. } => staged.push((
                    from,
                    Staged::Apply {
                        token,
                        proto: cmd,
                        args: args.as_slice().iter().map(|a| a.to_vec()).collect(),
                    },
                )),
                Op::Read { token, key, .. } => {
                    staged.push((from, Staged::Read { token, key: key.to_vec() }));
                }
                Op::Reply { token, outcome } => {
                    staged.push((
                        from,
                        Staged::Reply { token, outcome: OwnedOutcome::own(&outcome) },
                    ));
                }
                Op::Batch { ops } => {
                    for nested in ops {
                        stage(staged, from, nested);
                    }
                }
                // The M0 plane speaks Apply; a typed Write from a future peer
                // gets a typed refusal rather than silence.
                Op::Write { token, .. } => staged.push((from, Staged::Unsupported { token })),
            }
        }

        let mut staged: Vec<(CellId, Staged)> = Vec::new();
        let drained = self
            .shared
            .fabric
            .borrow_mut()
            .drain(FABRIC_DRAIN_MAX, |from, op| stage(&mut staged, from, op));
        if drained == 0 {
            return;
        }
        cx.note_fabric(drained as u64);

        for (from, item) in staged {
            match item {
                Staged::Reply { token, outcome } => {
                    // The drained reply already returned one data credit;
                    // wake one sender blocked on that destination.
                    self.shared.credit_waiters.wake_one(from);
                    if !self.shared.gate.complete(token.0, outcome) {
                        self.shared.fabric.borrow_mut().note_orphan_reply();
                    }
                }
                Staged::Apply { token, proto, args } => {
                    let argv: Vec<&[u8]> = args.iter().map(|a| &a[..]).collect();
                    let proto = if proto == 3 { Protocol::Resp3 } else { Protocol::Resp2 };
                    // Single-key DEL/EXISTS contributions stay typed for
                    // origin-side aggregation; everything else returns the
                    // raw RESP reply.
                    let counted = argv.len() == 2
                        && (argv[0].eq_ignore_ascii_case(b"DEL")
                            || argv[0].eq_ignore_ascii_case(b"EXISTS"));
                    if counted {
                        let del = argv[0].eq_ignore_ascii_case(b"DEL");
                        let n = self.shared.apply_counted(ExecOrigin::Fabric(from), del, argv[1]);
                        self.shared.fabric.borrow_mut().reply(from, token, &Outcome::Int(n));
                    } else {
                        let reply =
                            self.shared.execute_owned(ExecOrigin::Fabric(from), &argv, proto, 0);
                        self.shared.fabric.borrow_mut().reply(from, token, &Outcome::Bytes(&reply));
                    }
                }
                Staged::Read { token, key } => {
                    let value =
                        self.shared.store.borrow_mut().get(&key, cx.now).map(<[u8]>::to_vec);
                    let mut fabric = self.shared.fabric.borrow_mut();
                    match value {
                        Some(v) => fabric.reply(from, token, &Outcome::Bytes(&v)),
                        None => fabric.reply(from, token, &Outcome::Nil),
                    }
                }
                Staged::Unsupported { token } => {
                    self.shared.fabric.borrow_mut().reply(
                        from,
                        token,
                        &Outcome::Err(ErrCode::Unknown(0)),
                    );
                }
            }
        }
    }

    fn parse_execute(&mut self, cx: &mut LoopCx<'_>) {
        if !self.started {
            self.started = true;
            cx.push(IoOp::AcceptArm {
                listener: self.listener,
                token: CompletionToken::new(TokenClass::Accept, 0, 0),
            });
        }
        self.shared.now.set(cx.now);

        let inbox = core::mem::take(&mut self.inbox);
        for (key, buf, len) in inbox {
            let mut commands: u32 = 0;
            // First command that must defer to a pump (everything after it
            // defers too — replies are ordered per connection).
            let mut deferred: Vec<Vec<Vec<u8>>> = Vec::new();
            let mut spawn_first: Option<Vec<Vec<u8>>> = None;
            let mut protocol_error = false;
            {
                let mut conns = self.shared.conns.borrow_mut();
                let Some(conn) = conns.get_mut(key) else {
                    cx.pool.release(buf);
                    continue;
                };
                if conn.closing || conn.close_after_flush {
                    cx.pool.release(buf);
                    continue;
                }
                let data = &cx.pool.bytes(buf)[..len as usize];
                let pump_was_active = conn.pump_active;
                // Field split: the parser iterator borrows `conn.parser`
                // while execution writes `conn.out`/`conn.cx`.
                let Conn { parser, cx: conn_cx, out, .. } = &mut *conn;
                let mut iter = parser.feed(data);
                while let Some(parsed) = iter.next() {
                    match parsed {
                        Parsed::Command(argv) | Parsed::Inline(argv) => {
                            commands += 1;
                            let defer = pump_was_active
                                || spawn_first.is_some()
                                || !deferred.is_empty()
                                || self.needs_fabric(&argv);
                            if defer {
                                let owned: Vec<Vec<u8>> = argv.iter().map(<[u8]>::to_vec).collect();
                                if pump_was_active || spawn_first.is_some() {
                                    deferred.push(owned);
                                } else {
                                    spawn_first = Some(owned);
                                }
                            } else {
                                let argv_slices: Vec<&[u8]> = argv.iter().collect();
                                let before = out.len();
                                let now = self.shared.now.get();
                                execute(
                                    &argv,
                                    &mut self.shared.store.borrow_mut(),
                                    conn_cx,
                                    now,
                                    out,
                                );
                                self.shared.observer.borrow_mut().on_execute(
                                    self.shared.cell,
                                    ExecOrigin::Conn(key.slot, key.generation),
                                    &argv_slices,
                                    &out[before..],
                                    now,
                                );
                            }
                        }
                        Parsed::Incomplete => {}
                        Parsed::ProtocolError(e) => {
                            let mut w = RespWriter::new(out, conn_cx.proto);
                            w.error(&format!("ERR Protocol error: {e:?}"));
                            protocol_error = true;
                            break;
                        }
                    }
                }
                drop(iter);
                let conn = conns.get_mut(key).expect("conn checked above");
                if protocol_error {
                    conn.close_after_flush = true;
                }
                conn.queue.extend(deferred);
                if conn.queue.len() >= PENDING_HIGH_WATER && !conn.recv_disarmed {
                    conn.recv_disarmed = true;
                    cx.push(IoOp::RecvDisarm { fd: conn.fd });
                }
                if spawn_first.is_some() {
                    conn.pump_active = true;
                }
            }
            cx.pool.release(buf);
            cx.charge(GroupClass::Foreground, commands);
            if let Some(first) = spawn_first {
                self.spawn_pump(cx, key, first);
            }
        }
    }

    fn maintain(&mut self, _cx: &mut LoopCx<'_>) {
        let node = &self.shared.node;
        node.recv_dropped.set(self.shared.recv_dropped.get());
        node.fabric_rtt_p50_ns.set(self.shared.rtt_ns.borrow().percentile(50.0));
        let conns = self.shared.conns.borrow();
        node.connections.set(conns.live as u64);
        let bytes: usize = conns.slots.iter().flatten().map(Conn::state_bytes).sum::<usize>();
        node.conn_state_bytes.set(bytes as u64);
    }

    fn respond(&mut self, cx: &mut LoopCx<'_>) {
        let keys = self.shared.conns.borrow().keys();
        for key in keys {
            let mut close_now = false;
            self.shared.with_conn(key, |conn| {
                if conn.closing {
                    return;
                }
                if conn.rearm_recv {
                    conn.rearm_recv = false;
                    if conn.recv_disarmed {
                        conn.recv_disarmed = false;
                        cx.push(IoOp::RecvArm {
                            fd: conn.fd,
                            token: Self::token(TokenClass::Recv, key),
                        });
                    }
                }
                if !conn.out.is_empty()
                    && !conn.send_inflight
                    && let Some(buf) = cx.pool.try_lease(LeaseKind::Send)
                {
                    let n = conn.out.len().min(cx.pool.buf_size());
                    cx.pool.bytes_mut(buf)[..n].copy_from_slice(&conn.out[..n]);
                    conn.out.drain(..n);
                    conn.send_inflight = true;
                    cx.push(IoOp::Send {
                        fd: conn.fd,
                        buf,
                        len: n as u32,
                        token: Self::token(TokenClass::Send, key),
                    });
                }
                if conn.close_after_flush
                    && conn.out.is_empty()
                    && !conn.send_inflight
                    && !conn.pump_active
                {
                    close_now = true;
                }
            });
            if close_now {
                self.initiate_close(cx, key);
            }
        }
    }

    fn fabric_out(&mut self, cx: &mut LoopCx<'_>) -> bool {
        let mut fabric = self.shared.fabric.borrow_mut();
        let published = fabric.flush();
        if published > 0 {
            cx.note_fabric(published as u64);
        }
        fabric.doorbell_pending() || fabric.staged_frames() > 0
    }
}

/// Execute one owned command in connection order: local fast path, remote
/// single-owner `Apply`, or per-key DEL/EXISTS aggregation.
async fn run_one<O: PlaneObserver + 'static>(
    shared: &Rc<Shared<O>>,
    key: ConnKey,
    owned: &[Vec<u8>],
) {
    let argv: Vec<&[u8]> = owned.iter().map(|v| &v[..]).collect();
    let Some((proto, id)) = shared.with_conn(key, |c| (c.cx.proto, c.cx.id)) else { return };
    let origin = ExecOrigin::Conn(key.slot, key.generation);

    let meta = lookup(argv[0]);
    let well_formed = meta.is_some_and(|m| arity_ok(m, argv.len()));
    let reply: Vec<u8> = match meta {
        Some(meta)
            if well_formed
                && !shared.route_local_only
                && matches!(meta.id, CommandId::Del | CommandId::Exists) =>
        {
            // Per-key split: local keys count directly, remote keys ride
            // typed Apply replies. Order within the command is the argv
            // order (matches the single-store oracle).
            let del = meta.id == CommandId::Del;
            let name: &[u8] = if del { b"DEL" } else { b"EXISTS" };
            let mut acc: i64 = 0;
            for k in &argv[1..] {
                if shared.router.is_local(k, shared.cell) {
                    acc += shared.apply_counted(origin, del, k);
                } else {
                    let owner = shared.router.cell_of(SlotRouter::slot_of(k));
                    match remote_apply(shared, owner, proto, &[name, k]).await {
                        OwnedOutcome::Int(n) => acc += n,
                        other => {
                            debug_assert!(false, "counted apply returned {other:?}");
                        }
                    }
                }
            }
            let mut reply = Vec::new();
            RespWriter::new(&mut reply, proto).int(acc);
            reply
        }
        Some(meta)
            if well_formed && {
                !shared.route_local_only
                    && extract_keys_slices(meta, &argv)
                        .iter()
                        .any(|k| !shared.router.is_local(k, shared.cell))
            } =>
        {
            // Single-key remote command: ship the whole argv; the owner
            // executes and returns its raw RESP reply.
            let first_key = extract_keys_slices(meta, &argv)[0];
            let owner = shared.router.cell_of(SlotRouter::slot_of(first_key));
            match remote_apply(shared, owner, proto, &argv).await {
                OwnedOutcome::Bytes(reply) => reply,
                OwnedOutcome::Err(_) => {
                    let mut reply = Vec::new();
                    RespWriter::new(&mut reply, proto).error("ERR cross-cell execution failed");
                    reply
                }
                other => {
                    // Defensive: typed outcomes from a future peer.
                    let mut reply = Vec::new();
                    let mut w = RespWriter::new(&mut reply, proto);
                    match other {
                        OwnedOutcome::Ok => w.simple("OK"),
                        OwnedOutcome::Int(i) => w.int(i),
                        OwnedOutcome::Nil => w.null(),
                        OwnedOutcome::Bool(b) => w.bool(b),
                        OwnedOutcome::Bytes(_) | OwnedOutcome::Err(_) => unreachable!(),
                    }
                    reply
                }
            }
        }
        _ => shared.execute_owned(origin, &argv, proto, id),
    };

    shared.with_conn(key, |conn| conn.out.extend_from_slice(&reply));
}

/// Owned-slice twin of `extract_keys` (the wire helper wants an `ArgvRef`).
fn extract_keys_slices<'a>(meta: &inf_wire::CommandMeta, argv: &[&'a [u8]]) -> Vec<&'a [u8]> {
    let spec = meta.keys;
    if spec.first == 0 || argv.is_empty() {
        return Vec::new();
    }
    let last = if spec.last >= 0 {
        spec.last as usize
    } else {
        argv.len().saturating_sub(spec.last.unsigned_abs() as usize)
    };
    let mut keys = Vec::new();
    let mut i = usize::from(spec.first);
    while i <= last && i < argv.len() && spec.step > 0 {
        keys.push(argv[i]);
        i += usize::from(spec.step);
    }
    keys
}

/// Ship `argv` to `to` as an `Apply` and await the owner's reply, waiting
/// for fabric credits when exhausted (backpressure, never unbounded
/// queueing) and recording the hop round-trip.
async fn remote_apply<O: PlaneObserver + 'static>(
    shared: &Rc<Shared<O>>,
    to: CellId,
    proto: Protocol,
    argv: &[&[u8]],
) -> OwnedOutcome {
    let Some(args) = ApplyArgs::new(argv) else {
        return OwnedOutcome::Bytes({
            let mut reply = Vec::new();
            RespWriter::new(&mut reply, proto)
                .error("ERR too many arguments for cross-cell execution");
            reply
        });
    };
    let slot = SlotRouter::slot_of(argv[1]);
    let (token, waiter) = {
        let mut fabric = shared.fabric.borrow_mut();
        let token = fabric.next_token();
        // Register before sending: the reply may arrive in this very
        // iteration's FABRIC-IN; the gate parks early values.
        (token, shared.gate.waiter(token.0))
    };
    let proto_byte: u8 = match proto {
        Protocol::Resp3 => 3,
        Protocol::Resp2 => 2,
    };
    loop {
        let op = Op::Apply { token, slot, cmd: proto_byte, args };
        let sent = shared.fabric.borrow_mut().send(to, &op);
        match sent {
            Ok(()) => break,
            Err(SendError::NoCredit { .. }) => shared.credit_waiters.wait(to).await,
        }
    }
    let t0 = shared.now.get();
    let outcome = waiter.await;
    let rtt = shared.now.get().saturating_sub(t0);
    shared.rtt_ns.borrow_mut().record(rtt.0);
    outcome
}
