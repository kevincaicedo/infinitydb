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
//! - **Remote commands** run on a per-connection *pump* future. The pump
//!   dispatches commands in pipeline order with up to [`REMOTE_WINDOW`]
//!   remote ops in flight at once (the M0-E8 cross-cell remediation:
//!   one-hop-at-a-time execution was the 85% penalty), then emits replies
//!   strictly in command order — out-of-order completions park in the
//!   [`FabricGate`] until their turn. Sends always leave from the single
//!   pump, so per-key order rides the per-destination ring FIFO. The pump
//!   suspends on the front reply's gate and on a [`WaitList`] when fabric
//!   credits are exhausted. While a pump is active, later commands queue
//!   behind it; past a watermark the connection's recv is disarmed — credit
//!   backpressure reaches TCP (master plan §6.1). `HELLO` mutates
//!   connection state (protocol), so it dispatches only once every earlier
//!   reply has been emitted (a pipeline barrier).
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
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering, fence};

use inf_alloc::{BufferId, LeaseKind};
use inf_fabric::{ApplyArgs, CellFabric, ErrCode, FabricToken, Op, Outcome, SendError};
use inf_foundation::time::Nanos;
use inf_foundation::{CellId, LogHistogram};
use inf_runtime::GroupClass;
use inf_runtime::{
    CellPlane, Completion, CompletionResult, CompletionToken, FabricGate, GateWait, IoOp, LoopCx,
    RawFd, TokenClass, WaitList,
};
use inf_store::{CellStore, ExpiryBudget, SlotRouter};
use inf_wire::{
    ArgvRef, CommandId, ConnParser, Parsed, ParserLimits, Protocol, RespWriter, arity_ok,
    extract_keys, lookup,
};

use crate::exec::{ConnCx, NodeInfo, execute, execute_slices, stall_request};

/// Commands queued behind an active pump before recv is disarmed (bounded
/// everything — the backpressure watermark).
const PENDING_HIGH_WATER: usize = 1024;
/// Re-arm recv once the queue drains to this.
const PENDING_LOW_WATER: usize = 64;
/// Max fabric ops drained per FABRIC-IN step (bounded drain).
const FABRIC_DRAIN_MAX: usize = 1024;
/// Remote ops one connection may have in flight at once. Replies that land
/// out of order park in the `FabricGate` (≤ one value each) until emitted,
/// so this also bounds parked-reply memory per connection.
const REMOTE_WINDOW: usize = 32;
/// Replies (of any kind) awaiting in-order emission per connection; locals
/// executed eagerly behind a slow remote stage their bytes here.
const PENDING_REPLIES_MAX: usize = 256;
/// Reply-pool bounds: buffers kept per cell, and the largest buffer worth
/// keeping (anything bigger is freed, so one giant value can't pin memory).
const REPLY_POOL_MAX: usize = 256;
const REPLY_POOL_BUF_CAP: usize = 4096;
/// Hard cap on wheel fires per expiry MAINTAIN slice — the debt-aware
/// escalation (M1-S05) may multiply the deficit budget, never exceed this.
const MAX_EXPIRY_FIRES_PER_SLICE: u32 = 4096;
/// SCAN cursor layout (M1-S02): `{cell:16 | per-cell cursor:48}`.
const SCAN_CELL_SHIFT: u32 = 48;
const SCAN_LOCAL_MASK: u64 = (1 << SCAN_CELL_SHIFT) - 1;

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

// ---- deferred commands --------------------------------------------------------

/// One deferred command, flattened into a single allocation:
/// `[argc:u32][end_0:u32 … end_{argc-1}:u32][arg bytes …]` with absolute end
/// offsets. Replaces `Vec<Vec<u8>>` — 1+argc allocations per deferred
/// command was a top origin-side cost in the M0-R1 cross-cell profile.
struct OwnedCmd {
    buf: Vec<u8>,
}

impl OwnedCmd {
    fn from_argv(argv: &ArgvRef<'_>) -> OwnedCmd {
        let argc = argv.len();
        let head = 4 + 4 * argc;
        let total = head + (0..argc).map(|i| argv.arg(i).len()).sum::<usize>();
        let mut buf = Vec::with_capacity(total);
        buf.extend_from_slice(&u32::try_from(argc).expect("argc fits u32").to_le_bytes());
        let mut end = head;
        for i in 0..argc {
            end += argv.arg(i).len();
            buf.extend_from_slice(&u32::try_from(end).expect("cmd fits u32").to_le_bytes());
        }
        for i in 0..argc {
            buf.extend_from_slice(argv.arg(i));
        }
        OwnedCmd { buf }
    }

    fn argc(&self) -> usize {
        u32::from_le_bytes(self.buf[..4].try_into().expect("header")) as usize
    }

    fn end(&self, i: usize) -> usize {
        let at = 4 + 4 * i;
        u32::from_le_bytes(self.buf[at..at + 4].try_into().expect("ends table")) as usize
    }

    fn arg(&self, i: usize) -> &[u8] {
        let start = if i == 0 { 4 + 4 * self.argc() } else { self.end(i - 1) };
        &self.buf[start..self.end(i)]
    }

    /// Borrowed views over the flat buffer — the one remaining allocation
    /// per dispatched command (`extract_keys`/`ApplyArgs`/observer want
    /// `&[&[u8]]`).
    fn slices(&self) -> Vec<&[u8]> {
        (0..self.argc()).map(|i| self.arg(i)).collect()
    }

    fn mem(&self) -> usize {
        self.buf.capacity()
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
    queue: VecDeque<OwnedCmd>,
    recv_disarmed: bool,
    rearm_recv: bool,
}

impl Conn {
    fn state_bytes(&self) -> usize {
        size_of::<Conn>()
            + self.parser.buffered()
            + self.out.capacity()
            + self.queue.iter().map(OwnedCmd::mem).sum::<usize>()
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
    cells: u16,
    router: SlotRouter,
    /// Forces every key local — the cross-cell penalty A/B leg (§6 gate).
    route_local_only: bool,
    /// `DEBUG SLEEP` cell stall: connection parse/respond pause until this
    /// injected-clock instant (fabric service continues — deadlock safety).
    stall_until: Cell<Nanos>,
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
    /// Per-destination `(token, send time)` FIFO: replies return in send
    /// order per cell pair, so RTT is recorded at *delivery* (FABRIC-IN),
    /// not when the windowed pump finally awaits the parked value.
    rtt_sent: RefCell<Vec<VecDeque<(u64, Nanos)>>>,
    /// Recycled reply buffers (gate values, pump-local replies) — the
    /// remote path's per-op heap traffic was a top M0-R1 cost. Bounded by
    /// [`REPLY_POOL_MAX`]/[`REPLY_POOL_BUF_CAP`].
    reply_pool: RefCell<Vec<Vec<u8>>>,
    recv_dropped: Cell<u64>,
}

impl<O: PlaneObserver + 'static> Shared<O> {
    fn with_conn<R>(&self, key: ConnKey, f: impl FnOnce(&mut Conn) -> R) -> Option<R> {
        self.conns.borrow_mut().get_mut(key).map(f)
    }

    /// Executes owned argv locally (queued and remote-`Apply` paths),
    /// appending the reply to `out` (callers reuse scratch buffers — the
    /// owner side of a remote `Apply` is zero-allocation, M0-E8), and
    /// reports the apply point.
    fn execute_owned_into(
        &self,
        origin: ExecOrigin,
        argv: &[&[u8]],
        proto: Protocol,
        id: u64,
        out: &mut Vec<u8>,
    ) {
        let before = out.len();
        let mut cx = ConnCx { proto, id, node: Rc::clone(&self.node) };
        let now = self.now.get();
        execute_slices(argv, &mut self.store.borrow_mut(), &mut cx, now, out);
        self.observer.borrow_mut().on_execute(self.cell, origin, argv, &out[before..], now);
    }

    /// An empty reply buffer, recycled when possible.
    fn take_reply_buf(&self) -> Vec<u8> {
        let mut buf = self.reply_pool.borrow_mut().pop().unwrap_or_default();
        buf.clear();
        buf
    }

    /// Returns a reply buffer to the pool (bounded; oversized buffers drop).
    fn recycle_reply_buf(&self, buf: Vec<u8>) {
        if buf.capacity() == 0 || buf.capacity() > REPLY_POOL_BUF_CAP {
            return;
        }
        let mut pool = self.reply_pool.borrow_mut();
        if pool.len() < REPLY_POOL_MAX {
            pool.push(buf);
        }
    }

    /// Typed single-key DEL/UNLINK/EXISTS/TOUCH apply (local or owner side):
    /// the reply is the integer count contribution; observer sees the
    /// synthesized single-key command with its `:N` reply.
    fn apply_counted(&self, origin: ExecOrigin, name: &[u8], key: &[u8]) -> i64 {
        let now = self.now.get();
        let del = name.eq_ignore_ascii_case(b"DEL") || name.eq_ignore_ascii_case(b"UNLINK");
        let hit = {
            let mut store = self.store.borrow_mut();
            if del { store.del(key, now) } else { store.exists(key, now) }
        };
        let mut reply = Vec::new();
        RespWriter::new(&mut reply, Protocol::Resp2).int(i64::from(hit));
        self.observer.borrow_mut().on_execute(self.cell, origin, &[name, key], &reply, now);
        i64::from(hit)
    }

    /// Typed DBSIZE apply (scatter contribution, M1-S02).
    fn apply_dbsize(&self, origin: ExecOrigin) -> i64 {
        let now = self.now.get();
        let len = self.store.borrow().len() as i64;
        let mut reply = Vec::new();
        RespWriter::new(&mut reply, Protocol::Resp2).int(len);
        self.observer.borrow_mut().on_execute(self.cell, origin, &[b"DBSIZE"], &reply, now);
        len
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
    /// Reusable FABRIC-IN scratch: owner-side reply bytes for this drain.
    reply_scratch: Vec<u8>,
    /// Reusable FABRIC-IN scratch: replies staged while the fabric is
    /// borrowed by `drain`, sent the moment it ends.
    staged_replies: Vec<(CellId, FabricToken, StagedReply)>,
    /// Doorbell-wakeup park board (M0-R1): this cell sets `[cell]` in the
    /// park handshake; peers read it at flush. Single-writer per slot — the
    /// same blessed class as the fabric doorbells, NOT shared mutable
    /// data-plane state.
    park_flags: Option<Arc<Vec<AtomicBool>>>,
    /// `expiry_debt` backlog (ms the wheel trails `now`) from the previous
    /// expiry slice — drives the M1-S05 debt-aware budget escalation.
    expiry_lag: u64,
}

/// An owner-side reply produced during the FABRIC-IN drain (ranges index
/// into the reply scratch buffer).
enum StagedReply {
    Bytes(usize, usize),
    Int(i64),
    Nil,
    /// Typed refusal for an op the M0 plane does not speak.
    Refused,
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
                cells,
                router: SlotRouter::new_contiguous(cells),
                route_local_only,
                stall_until: Cell::new(Nanos(0)),
                store: RefCell::new(store),
                fabric: RefCell::new(fabric),
                conns: RefCell::new(ConnSlab::default()),
                gate: FabricGate::new(),
                credit_waiters: WaitList::new(),
                observer: RefCell::new(observer),
                node,
                now: Cell::new(Nanos(0)),
                rtt_ns: RefCell::new(LogHistogram::new()),
                rtt_sent: RefCell::new(vec![VecDeque::new(); usize::from(cells)]),
                reply_pool: RefCell::new(Vec::new()),
                recv_dropped: Cell::new(0),
            }),
            listener,
            started: false,
            inbox: Vec::new(),
            reply_scratch: Vec::new(),
            staged_replies: Vec::new(),
            park_flags: None,
            expiry_lag: 0,
        }
    }

    /// Wires this plane's slot of the doorbell-wakeup park board (the same
    /// `Arc` goes to every cell's fabric via `CellFabric::set_wakeups`).
    pub fn set_park_flags(&mut self, flags: Arc<Vec<AtomicBool>>) {
        self.park_flags = Some(flags);
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

    /// True when a well-formed command must run on the pump: at least one
    /// key is owned by another cell, or it is a keyspace-wide scatter
    /// command on a multi-cell node (M1-S02).
    fn needs_fabric(&self, argv: &ArgvRef<'_>) -> bool {
        if self.shared.route_local_only {
            return false;
        }
        let Some(meta) = lookup(argv.arg(0)) else { return false };
        if !arity_ok(meta, argv.len()) {
            return false;
        }
        if self.shared.cells > 1 && is_scatter(meta.id) {
            return true;
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

    /// Spawn the per-connection windowed pump with its first command.
    fn spawn_pump(&self, cx: &mut LoopCx<'_>, key: ConnKey, first: OwnedCmd) {
        let shared = Rc::clone(&self.shared);
        let _ = cx.executor.poll_immediate(pump(shared, key, first));
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
                let node = &self.shared.node;
                node.total_connections.set(node.total_connections.get() + 1);
                // Peer address capture is a recorded deviation (CLIENT LIST
                // placeholder) until the accept path carries peernames.
                node.clients.borrow_mut().register(id, "0.0.0.0:0".to_string(), cx.now.as_millis());
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
                let id = (u64::from(key.slot) << 32) | u64::from(key.generation);
                self.shared.node.clients.borrow_mut().unregister(id);
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

    fn before_park(&mut self) -> bool {
        let Some(flags) = &self.park_flags else { return false };
        let me = usize::from(self.shared.cell.0);
        flags[me].store(true, Ordering::Relaxed);
        // Pairs with the producer's ring → fence → parked-flag load: either
        // this final check sees the doorbell, or the producer sees the flag
        // and wakes us. A doubly-missed wake degrades to the park timeout,
        // never a hang.
        fence(Ordering::SeqCst);
        if self.shared.fabric.borrow().doorbell_pending() {
            flags[me].store(false, Ordering::Relaxed);
            return true;
        }
        false
    }

    fn fabric_in(&mut self, cx: &mut LoopCx<'_>) {
        if let Some(flags) = &self.park_flags {
            flags[usize::from(self.shared.cell.0)].store(false, Ordering::Relaxed);
        }
        self.shared.now.set(cx.now);
        // Ops execute *during* the drain over their borrowed ring payloads —
        // the owner side of a remote `Apply` is zero-allocation (M0-E8: the
        // owned-staging copies dominated the cross-cell profile). Only the
        // replies wait: the fabric is mutably borrowed by `drain`, so their
        // bytes land in the reusable scratch and ship the moment it ends.
        self.reply_scratch.clear();
        self.staged_replies.clear();
        let shared = &self.shared;
        let scratch = &mut self.reply_scratch;
        let staged = &mut self.staged_replies;
        let mut orphans: u64 = 0;
        let now = cx.now;
        let drained = shared.fabric.borrow_mut().drain(FABRIC_DRAIN_MAX, |from, op| {
            handle_fabric_op(shared, now, from, op, scratch, staged, &mut orphans);
        });
        if drained == 0 {
            return;
        }
        cx.note_fabric(drained as u64);

        let mut fabric = self.shared.fabric.borrow_mut();
        for _ in 0..orphans {
            fabric.note_orphan_reply();
        }
        let mut had_replies = false;
        for (to, token, reply) in self.staged_replies.drain(..) {
            had_replies = true;
            match reply {
                StagedReply::Bytes(start, end) => {
                    fabric.reply(to, token, &Outcome::Bytes(&self.reply_scratch[start..end]));
                }
                StagedReply::Int(n) => fabric.reply(to, token, &Outcome::Int(n)),
                StagedReply::Nil => fabric.reply(to, token, &Outcome::Nil),
                StagedReply::Refused => {
                    fabric.reply(to, token, &Outcome::Err(ErrCode::Unknown(0)));
                }
            }
        }
        // Publish replies NOW instead of at FABRIC-OUT: the origin is
        // blocked on them, and waiting for step 8 adds most of an iteration
        // to every hop RTT (M0-R1 latency finding — hops were
        // window-latency-bound, not just CPU-bound).
        if had_replies {
            let published = fabric.flush();
            if published > 0 {
                cx.note_fabric(published as u64);
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
        // DEBUG SLEEP stall: connection processing pauses (inbox buffers
        // hold; pool pressure degrades to RecvDropped, never blocks the
        // thread); FABRIC-IN keeps serving peers.
        if cx.now < self.shared.stall_until.get() {
            return;
        }

        let inbox = core::mem::take(&mut self.inbox);
        for (key, buf, len) in inbox {
            let mut commands: u32 = 0;
            // First command that must defer to a pump (everything after it
            // defers too — replies are ordered per connection).
            let mut deferred: Vec<OwnedCmd> = Vec::new();
            let mut spawn_first: Option<OwnedCmd> = None;
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
                                let owned = OwnedCmd::from_argv(&argv);
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
                                if let Some(dur) = stall_request(&argv_slices) {
                                    self.shared.stall_until.set(now.saturating_add(dur));
                                }
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

    fn maintain(&mut self, cx: &mut LoopCx<'_>) {
        self.shared.now.set(cx.now);
        // ---- expiry slice (M1-S05): wheel ticks under the Maintenance
        // deficit budget; the `expiry_debt` lag escalates the slice (×1..16,
        // hard-capped) so storms drain on idle headroom while foreground
        // latency stays protected by the deficit scheduler.
        let budget = cx.budget(GroupClass::Maintenance);
        if budget > 0 {
            let escalation = (self.expiry_lag / 1024).min(15) as u32 + 1;
            let max_fires = budget.saturating_mul(escalation).min(MAX_EXPIRY_FIRES_PER_SLICE);
            let stats = self.shared.store.borrow_mut().expire_tick(
                cx.now,
                ExpiryBudget { max_fires, max_steps: max_fires.saturating_mul(8).max(4096) },
            );
            self.expiry_lag = stats.lag_ms;
            let units =
                (stats.reaped + stats.stale).min(u64::from(u32::MAX)) as u32 + stats.steps / 64;
            if units > 0 {
                cx.charge(GroupClass::Maintenance, units);
            }
        }
        // ---- CLIENT KILL sweep: ids encode {slot:32 | generation:32}, so
        // the registry mark maps straight back to the conn slab.
        let kills = self.shared.node.clients.borrow_mut().take_kill_requests();
        for id in kills {
            let key = ConnKey { slot: (id >> 32) as u32, generation: id as u32 };
            self.initiate_close(cx, key);
        }
        // ---- stats flush
        let node = &self.shared.node;
        node.recv_dropped.set(self.shared.recv_dropped.get());
        node.fabric_rtt_p50_ns.set(self.shared.rtt_ns.borrow().percentile(50.0));
        let conns = self.shared.conns.borrow();
        node.connections.set(conns.live as u64);
        let bytes: usize = conns.slots.iter().flatten().map(Conn::state_bytes).sum::<usize>();
        node.conn_state_bytes.set(bytes as u64);
    }

    fn respond(&mut self, cx: &mut LoopCx<'_>) {
        // Replies (including DEBUG SLEEP's own +OK) hold until a stall ends.
        if cx.now < self.shared.stall_until.get() {
            return;
        }
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

/// One drained fabric op, handled while its payload still borrows the ring
/// slot (zero copies in): `Reply` completes the origin-side gate inline;
/// `Apply`/`Read` execute against the store and stage their reply bytes
/// into `scratch` (the fabric itself is borrowed by the drain — replies
/// ship right after it ends). `orphans` counts gate-less replies for the
/// fabric tripwire.
fn handle_fabric_op<O: PlaneObserver + 'static>(
    shared: &Shared<O>,
    now: Nanos,
    from: CellId,
    op: Op<'_>,
    scratch: &mut Vec<u8>,
    staged: &mut Vec<(CellId, FabricToken, StagedReply)>,
    orphans: &mut u64,
) {
    match op {
        Op::Reply { token, outcome } => {
            // Delivery-time hop RTT: replies return in send order per cell
            // pair, so the front send-time entry is this reply's (recording
            // at the pump's await would charge head-of-line parking to the
            // fabric).
            if let Some((sent_token, t0)) =
                shared.rtt_sent.borrow_mut()[usize::from(from.0)].pop_front()
            {
                debug_assert_eq!(sent_token, token.0, "reply order diverged from sends");
                shared.rtt_ns.borrow_mut().record(now.saturating_sub(t0).0);
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
        Op::Apply { token, cmd, args, .. } => {
            let argv = args.as_slice();
            let proto = if cmd == 3 { Protocol::Resp3 } else { Protocol::Resp2 };
            // Single-key DEL/UNLINK/EXISTS/TOUCH contributions and DBSIZE
            // stay typed for origin-side aggregation; everything else
            // returns the raw RESP reply.
            let counted = argv.len() == 2
                && [&b"DEL"[..], b"UNLINK", b"EXISTS", b"TOUCH"]
                    .iter()
                    .any(|n| argv[0].eq_ignore_ascii_case(n));
            if counted {
                let n = shared.apply_counted(ExecOrigin::Fabric(from), argv[0], argv[1]);
                staged.push((from, token, StagedReply::Int(n)));
            } else if argv.len() == 1 && argv[0].eq_ignore_ascii_case(b"DBSIZE") {
                let n = shared.apply_dbsize(ExecOrigin::Fabric(from));
                staged.push((from, token, StagedReply::Int(n)));
            } else {
                let start = scratch.len();
                shared.execute_owned_into(ExecOrigin::Fabric(from), argv, proto, 0, scratch);
                staged.push((from, token, StagedReply::Bytes(start, scratch.len())));
            }
        }
        Op::Read { token, key, .. } => {
            let start = scratch.len();
            let hit = match shared.store.borrow_mut().get(key, now) {
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
        Op::Batch { ops } => {
            for nested in ops {
                handle_fabric_op(shared, now, from, nested, scratch, staged, orphans);
            }
        }
        // The M0 plane speaks Apply; a typed Write from a future peer gets
        // a typed refusal rather than silence.
        Op::Write { token, .. } => staged.push((from, token, StagedReply::Refused)),
    }
}

/// One MGET position: rendered locally at dispatch, or one remote `GET`.
enum GatherPart {
    Done(Vec<u8>),
    Wait(GateWait<u64, OwnedOutcome>),
}

/// A reply slot awaiting its in-order turn on the wire.
enum PendingReply {
    /// Executed (locally or refused) at dispatch; bytes wait their turn.
    Done(Vec<u8>),
    /// One remote `Apply` in flight; the owner's raw RESP reply parks in
    /// the gate if it lands before its turn.
    Remote { waiter: GateWait<u64, OwnedOutcome>, proto: Protocol },
    /// Split DEL/UNLINK/EXISTS/TOUCH (and scattered DBSIZE): locally-counted
    /// contributions in `acc`, remote per-key contributions in flight.
    Counted { waiters: Vec<GateWait<u64, OwnedOutcome>>, acc: i64, proto: Protocol },
    /// Split MGET: per-key replies reassemble into one array in argv order.
    Gather { parts: Vec<GatherPart>, proto: Protocol },
    /// Fanned MSET / scattered FLUSH: all legs must come back `+OK` (the
    /// first error leg wins the reply otherwise).
    AllOk { waiters: Vec<GateWait<u64, OwnedOutcome>>, proto: Protocol },
}

/// What the pump found when it asked the connection for more work.
enum Popped {
    Cmd(OwnedCmd),
    /// Queue empty but replies are still pending — keep emitting.
    Empty,
    /// Queue empty, nothing pending (pump deactivated inside the conn
    /// borrow) or the connection is gone: the pump is done.
    Finished,
}

fn pop_or_quiesce<O: PlaneObserver + 'static>(
    shared: &Shared<O>,
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
/// every later reply serializes under).
fn is_conn_state(owned: &OwnedCmd) -> bool {
    lookup(owned.arg(0)).is_some_and(|m| m.id == CommandId::Hello)
}

/// The per-connection pump: dispatch commands in pipeline order with up to
/// [`REMOTE_WINDOW`] remote ops in flight, emit replies strictly in command
/// order. Suspends only on the front reply's gate and on fabric credits;
/// out-of-order completions park in the gate until their turn.
async fn pump<O: PlaneObserver + 'static>(shared: Rc<Shared<O>>, key: ConnKey, first: OwnedCmd) {
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
            if !dispatch_one(&shared, key, &cmd, &mut pending, &mut inflight).await {
                return; // connection is gone
            }
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
            PendingReply::Counted { waiters, mut acc, proto } => {
                for waiter in waiters {
                    match waiter.await {
                        OwnedOutcome::Int(n) => acc += n,
                        other => debug_assert!(false, "counted apply returned {other:?}"),
                    }
                    inflight -= 1;
                }
                let mut reply = shared.take_reply_buf();
                RespWriter::new(&mut reply, proto).int(acc);
                reply
            }
            PendingReply::Gather { parts, proto } => {
                let mut reply = shared.take_reply_buf();
                RespWriter::new(&mut reply, proto).array_header(parts.len());
                for part in parts {
                    match part {
                        GatherPart::Done(bytes) => {
                            reply.extend_from_slice(&bytes);
                            shared.recycle_reply_buf(bytes);
                        }
                        GatherPart::Wait(waiter) => {
                            let outcome = waiter.await;
                            inflight -= 1;
                            match outcome {
                                OwnedOutcome::Bytes(bytes) => {
                                    reply.extend_from_slice(&bytes);
                                    shared.recycle_reply_buf(bytes);
                                }
                                _ => RespWriter::new(&mut reply, proto).null(),
                            }
                        }
                    }
                }
                reply
            }
            PendingReply::AllOk { waiters, proto } => {
                let mut failure: Option<Vec<u8>> = None;
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
/// multi-cell node (M1-S02).
fn is_scatter(id: CommandId) -> bool {
    matches!(
        id,
        CommandId::Dbsize
            | CommandId::Keys
            | CommandId::Scan
            | CommandId::Flushdb
            | CommandId::Flushall
            | CommandId::Randomkey
    )
}

/// Dispatch one command: execute locally into a `Done` slot, or ship its
/// remote ops (suspending only on fabric credits — backpressure, never
/// unbounded queueing) and stage the reply waiter. Multi-key commands split
/// per key; RENAME/RENAMENX/COPY across two owners and keyspace-wide
/// commands run as inline fabric programs (M1-S02). Returns `false` when
/// the connection is gone.
async fn dispatch_one<O: PlaneObserver + 'static>(
    shared: &Rc<Shared<O>>,
    key: ConnKey,
    owned: &OwnedCmd,
    pending: &mut VecDeque<PendingReply>,
    inflight: &mut usize,
) -> bool {
    let argv: Vec<&[u8]> = owned.slices();
    let Some((proto, id)) = shared.with_conn(key, |c| (c.cx.proto, c.cx.id)) else { return false };
    let origin = ExecOrigin::Conn(key.slot, key.generation);

    let meta = lookup(argv[0]);
    let well_formed = meta.is_some_and(|m| arity_ok(m, argv.len()));
    let has_remote_key = |meta| {
        !shared.route_local_only
            && extract_keys_slices(meta, &argv)
                .iter()
                .any(|k| !shared.router.is_local(k, shared.cell))
    };
    let owner_of = |k: &[u8]| shared.router.cell_of(SlotRouter::slot_of(k));
    match meta {
        Some(meta)
            if well_formed
                && is_scatter(meta.id)
                && shared.cells > 1
                && !shared.route_local_only =>
        {
            match meta.id {
                CommandId::Dbsize => {
                    let acc = shared.apply_dbsize(origin);
                    let mut waiters = Vec::new();
                    for cell in peer_cells(shared) {
                        if let Ok(waiter) = send_apply(shared, cell, proto, &[b"DBSIZE"]).await {
                            waiters.push(waiter);
                            *inflight += 1;
                        }
                    }
                    pending.push_back(PendingReply::Counted { waiters, acc, proto });
                }
                CommandId::Flushdb | CommandId::Flushall => {
                    // The local leg validates options and flushes; an error
                    // reply short-circuits the fan-out.
                    let local = run_local(shared, origin, proto, id, &argv);
                    if local.first() == Some(&b'-') {
                        pending.push_back(PendingReply::Done(local));
                    } else {
                        shared.recycle_reply_buf(local);
                        let mut waiters = Vec::new();
                        for cell in peer_cells(shared) {
                            if let Ok(waiter) = send_apply(shared, cell, proto, &argv).await {
                                waiters.push(waiter);
                                *inflight += 1;
                            }
                        }
                        pending.push_back(PendingReply::AllOk { waiters, proto });
                    }
                }
                CommandId::Keys => {
                    let reply = program_keys(shared, origin, proto, id, &argv).await;
                    pending.push_back(PendingReply::Done(reply));
                }
                CommandId::Scan => {
                    let reply = program_scan(shared, origin, proto, id, &argv).await;
                    pending.push_back(PendingReply::Done(reply));
                }
                CommandId::Randomkey => {
                    let reply = program_randomkey(shared, origin, proto, id, &argv).await;
                    pending.push_back(PendingReply::Done(reply));
                }
                _ => unreachable!("is_scatter covers exactly the arms above"),
            }
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
                    acc += shared.apply_counted(origin, name, k);
                } else {
                    match send_apply(shared, owner_of(k), proto, &[name, k]).await {
                        Ok(waiter) => {
                            waiters.push(waiter);
                            *inflight += 1;
                        }
                        Err(_) => debug_assert!(false, "2-arg apply exceeded ApplyArgs"),
                    }
                }
            }
            pending.push_back(PendingReply::Counted { waiters, acc, proto });
        }
        Some(meta) if well_formed && meta.id == CommandId::Mget && has_remote_key(meta) => {
            // Gather: every position resolves independently, replies
            // reassemble into one array in argv order (the §6.2 "pipelined,
            // not serialized" shape; per-destination BatchOp coalescing is
            // the M1-S17 story).
            let mut parts = Vec::with_capacity(argv.len() - 1);
            for k in &argv[1..] {
                if shared.router.is_local(k, shared.cell) {
                    let mut buf = shared.take_reply_buf();
                    shared.execute_owned_into(origin, &[b"GET", k], proto, id, &mut buf);
                    parts.push(GatherPart::Done(buf));
                } else {
                    match send_apply(shared, owner_of(k), proto, &[b"GET", k]).await {
                        Ok(waiter) => {
                            parts.push(GatherPart::Wait(waiter));
                            *inflight += 1;
                        }
                        Err(refusal) => parts.push(GatherPart::Done(refusal)),
                    }
                }
            }
            pending.push_back(PendingReply::Gather { parts, proto });
        }
        Some(meta) if well_formed && meta.id == CommandId::Mset && has_remote_key(meta) => {
            if argv.len().is_multiple_of(2) {
                pending.push_back(PendingReply::Done(error_reply(
                    shared,
                    proto,
                    "ERR wrong number of arguments for 'mset' command",
                )));
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
                    pending.push_back(PendingReply::Done(error));
                } else {
                    let mut waiters = Vec::new();
                    let mut i = 1;
                    while i < argv.len() {
                        if !shared.router.is_local(argv[i], shared.cell)
                            && let Ok(waiter) = send_apply(
                                shared,
                                owner_of(argv[i]),
                                proto,
                                &[b"SET", argv[i], argv[i + 1]],
                            )
                            .await
                        {
                            waiters.push(waiter);
                            *inflight += 1;
                        }
                        i += 2;
                    }
                    pending.push_back(PendingReply::AllOk { waiters, proto });
                }
            }
        }
        Some(meta) if well_formed && meta.id == CommandId::Msetnx && has_remote_key(meta) => {
            let reply = program_msetnx(shared, origin, proto, id, &argv).await;
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
            let reply = program_move(shared, origin, proto, id, meta.id, &argv).await;
            pending.push_back(PendingReply::Done(reply));
        }
        Some(meta) if well_formed && has_remote_key(meta) => {
            // Single-owner remote command: ship the whole argv; the owner
            // executes and returns its raw RESP reply.
            let first_key = extract_keys_slices(meta, &argv)[0];
            let owner = owner_of(first_key);
            match send_apply(shared, owner, proto, &argv).await {
                Ok(waiter) => {
                    *inflight += 1;
                    pending.push_back(PendingReply::Remote { waiter, proto });
                }
                Err(refusal) => pending.push_back(PendingReply::Done(refusal)),
            }
        }
        _ => {
            let mut reply = shared.take_reply_buf();
            if is_conn_state(owned) {
                // Execute under a cx mirroring the live connection, then
                // write the negotiated protocol back — the M0 pump dropped
                // HELLO's proto switch on queued pipelines (temp-cx bug,
                // found extending the surface; ledger entry).
                let Some(mut live) = shared.with_conn(key, |c| ConnCx {
                    proto: c.cx.proto,
                    id: c.cx.id,
                    node: Rc::clone(&shared.node),
                }) else {
                    return false;
                };
                let now = shared.now.get();
                execute_slices(&argv, &mut shared.store.borrow_mut(), &mut live, now, &mut reply);
                shared.observer.borrow_mut().on_execute(shared.cell, origin, &argv, &reply, now);
                shared.with_conn(key, |c| c.cx.proto = live.proto);
            } else {
                shared.execute_owned_into(origin, &argv, proto, id, &mut reply);
                if let Some(dur) = stall_request(&argv) {
                    shared.stall_until.set(shared.now.get().saturating_add(dur));
                }
            }
            pending.push_back(PendingReply::Done(reply));
        }
    }
    true
}

/// Render an owner's outcome as the RESP reply for a whole-argv `Apply`
/// (buffers come from and return to the cell's reply pool).
fn render_outcome<O: PlaneObserver + 'static>(
    shared: &Shared<O>,
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
                OwnedOutcome::Bytes(_) | OwnedOutcome::Err(_) => unreachable!(),
            }
            reply
        }
    }
}

// ---- fabric-program helpers (M1-S02) -------------------------------------------

/// All cells except this one (scatter targets).
fn peer_cells<O: PlaneObserver + 'static>(shared: &Shared<O>) -> Vec<CellId> {
    (0..shared.cells).map(CellId).filter(|c| c.0 != shared.cell.0).collect()
}

fn error_reply<O: PlaneObserver + 'static>(
    shared: &Shared<O>,
    proto: Protocol,
    text: &str,
) -> Vec<u8> {
    let mut reply = shared.take_reply_buf();
    RespWriter::new(&mut reply, proto).error(text);
    reply
}

fn int_reply<O: PlaneObserver + 'static>(shared: &Shared<O>, proto: Protocol, n: i64) -> Vec<u8> {
    let mut reply = shared.take_reply_buf();
    RespWriter::new(&mut reply, proto).int(n);
    reply
}

fn simple_reply<O: PlaneObserver + 'static>(
    shared: &Shared<O>,
    proto: Protocol,
    text: &str,
) -> Vec<u8> {
    let mut reply = shared.take_reply_buf();
    RespWriter::new(&mut reply, proto).simple(text);
    reply
}

fn run_local<O: PlaneObserver + 'static>(
    shared: &Shared<O>,
    origin: ExecOrigin,
    proto: Protocol,
    id: u64,
    argv: &[&[u8]],
) -> Vec<u8> {
    let mut reply = shared.take_reply_buf();
    shared.execute_owned_into(origin, argv, proto, id, &mut reply);
    reply
}

/// One program step: execute `argv` on `cell` (locally or via Apply) and
/// return its raw RESP reply bytes.
async fn run_on<O: PlaneObserver + 'static>(
    shared: &Rc<Shared<O>>,
    origin: ExecOrigin,
    cell: CellId,
    proto: Protocol,
    id: u64,
    argv: &[&[u8]],
) -> Vec<u8> {
    if cell.0 == shared.cell.0 {
        return run_local(shared, origin, proto, id, argv);
    }
    match send_apply(shared, cell, proto, argv).await {
        Ok(waiter) => match waiter.await {
            OwnedOutcome::Bytes(bytes) => bytes,
            outcome => render_outcome(shared, outcome, proto),
        },
        Err(refusal) => refusal,
    }
}

/// One typed counted step (EXISTS/DEL shape) on `cell`.
async fn count_on<O: PlaneObserver + 'static>(
    shared: &Rc<Shared<O>>,
    origin: ExecOrigin,
    cell: CellId,
    proto: Protocol,
    name: &[u8],
    key: &[u8],
) -> Result<i64, Vec<u8>> {
    if cell.0 == shared.cell.0 {
        return Ok(shared.apply_counted(origin, name, key));
    }
    match send_apply(shared, cell, Protocol::Resp2, &[name, key]).await {
        Ok(waiter) => match waiter.await {
            OwnedOutcome::Int(n) => Ok(n),
            _ => Err(error_reply(shared, proto, "ERR cross-cell execution failed")),
        },
        Err(refusal) => Err(refusal),
    }
}

/// Cross-cell MSETNX: existence sweep, then the SET fan. Recorded deviation
/// (compat matrix): check-then-set is not atomic across cells until M4
/// transactions; single-cell MSETNX stays exact.
async fn program_msetnx<O: PlaneObserver + 'static>(
    shared: &Rc<Shared<O>>,
    origin: ExecOrigin,
    proto: Protocol,
    id: u64,
    argv: &[&[u8]],
) -> Vec<u8> {
    if argv.len().is_multiple_of(2) {
        return error_reply(shared, proto, "ERR wrong number of arguments for 'msetnx' command");
    }
    let mut i = 1;
    while i < argv.len() {
        let owner = shared.router.cell_of(SlotRouter::slot_of(argv[i]));
        match count_on(shared, origin, owner, proto, b"EXISTS", argv[i]).await {
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
            run_on(shared, origin, owner, Protocol::Resp2, id, &[b"SET", argv[i], argv[i + 1]])
                .await;
        if reply.first() == Some(&b'-') {
            return reply;
        }
        shared.recycle_reply_buf(reply);
        i += 2;
    }
    int_reply(shared, proto, 1)
}

/// Cross-owner RENAME/RENAMENX/COPY: `INF.TAKE`/`INF.PEEK` at the source
/// (atomic there), `SET [PX] [NX]` at the destination. Atomic per cell;
/// full cross-cell atomicity arrives with M4 transactions (documented). The
/// TTL transfers as relative milliseconds — the hop skew is microseconds
/// (recorded deviation vs Redis's absolute deadline).
async fn program_move<O: PlaneObserver + 'static>(
    shared: &Rc<Shared<O>>,
    origin: ExecOrigin,
    proto: Protocol,
    id: u64,
    cmd: CommandId,
    argv: &[&[u8]],
) -> Vec<u8> {
    let (src, dst) = (argv[1], argv[2]);
    let src_owner = shared.router.cell_of(SlotRouter::slot_of(src));
    let dst_owner = shared.router.cell_of(SlotRouter::slot_of(dst));
    let mut replace = false;
    if cmd == CommandId::Copy {
        let mut i = 3;
        while i < argv.len() {
            let opt = argv[i];
            if opt.eq_ignore_ascii_case(b"REPLACE") {
                replace = true;
            } else if opt.eq_ignore_ascii_case(b"DB") && i + 1 < argv.len() {
                if argv[i + 1] != b"0" {
                    return error_reply(shared, proto, "ERR DB index is out of range");
                }
                i += 1;
            } else {
                return error_reply(shared, proto, "ERR syntax error");
            }
            i += 1;
        }
    }
    if cmd == CommandId::Renamenx {
        // Pre-check at the destination (the window between this check and
        // the SET below is the documented non-atomicity).
        match count_on(shared, origin, dst_owner, proto, b"EXISTS", dst).await {
            Ok(0) => {}
            Ok(_) => return int_reply(shared, proto, 0),
            Err(error) => return error,
        }
    }
    let probe: &[u8] = if cmd == CommandId::Copy { b"INF.PEEK" } else { b"INF.TAKE" };
    let raw = run_on(shared, origin, src_owner, Protocol::Resp2, id, &[probe, src]).await;
    if raw.first() == Some(&b'-') {
        return raw;
    }
    let Some(taken) = parse_take_reply(&raw) else {
        return error_reply(shared, proto, "ERR cross-cell program reply malformed");
    };
    shared.recycle_reply_buf(raw);
    let Some((value, pttl)) = taken else {
        return match cmd {
            CommandId::Copy => int_reply(shared, proto, 0),
            _ => error_reply(shared, proto, "ERR no such key"),
        };
    };
    let mut ttl_buf = [0u8; 20];
    let mut put: Vec<&[u8]> = vec![b"SET", dst, &value];
    if pttl >= 0 {
        put.push(b"PX");
        put.push(crate::exec::fmt_u64(&mut ttl_buf, pttl as u64));
    }
    if cmd == CommandId::Copy && !replace {
        put.push(b"NX"); // TOCTOU-free destination guard
    }
    let reply = run_on(shared, origin, dst_owner, Protocol::Resp2, id, &put).await;
    if reply.first() == Some(&b'-') {
        return reply;
    }
    let set_applied = reply.starts_with(b"+OK");
    shared.recycle_reply_buf(reply);
    match cmd {
        CommandId::Rename => simple_reply(shared, proto, "OK"),
        CommandId::Renamenx => int_reply(shared, proto, 1),
        _ => int_reply(shared, proto, i64::from(set_applied)),
    }
}

/// Scattered KEYS: local sweep + one Apply per peer, arrays merged by
/// header arithmetic (bodies concatenate untouched).
async fn program_keys<O: PlaneObserver + 'static>(
    shared: &Rc<Shared<O>>,
    origin: ExecOrigin,
    proto: Protocol,
    id: u64,
    argv: &[&[u8]],
) -> Vec<u8> {
    let local = run_local(shared, origin, proto, id, argv);
    let Some((mut total, local_off)) = parse_array_header(&local) else {
        return local; // error passthrough
    };
    let mut waiters = Vec::new();
    for cell in peer_cells(shared) {
        match send_apply(shared, cell, proto, argv).await {
            Ok(waiter) => waiters.push(waiter),
            Err(refusal) => return refusal,
        }
    }
    let mut bodies: Vec<(Vec<u8>, usize)> = Vec::new();
    for waiter in waiters {
        match waiter.await {
            OwnedOutcome::Bytes(bytes) => {
                let Some((n, off)) = parse_array_header(&bytes) else {
                    return bytes; // peer error passthrough
                };
                total += n;
                bodies.push((bytes, off));
            }
            _ => return error_reply(shared, proto, "ERR cross-cell execution failed"),
        }
    }
    let mut reply = shared.take_reply_buf();
    RespWriter::new(&mut reply, proto).array_header(total);
    reply.extend_from_slice(&local[local_off..]);
    shared.recycle_reply_buf(local);
    for (bytes, off) in bodies {
        reply.extend_from_slice(&bytes[off..]);
        shared.recycle_reply_buf(bytes);
    }
    reply
}

/// Scattered SCAN: the cursor packs `{cell:16 | per-cell cursor:48}`; one
/// cell serves each call, the cursor hops to the next cell when a cell's
/// local scan wraps.
async fn program_scan<O: PlaneObserver + 'static>(
    shared: &Rc<Shared<O>>,
    origin: ExecOrigin,
    proto: Protocol,
    id: u64,
    argv: &[&[u8]],
) -> Vec<u8> {
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
    let raw = run_on(shared, origin, CellId(target), proto, id, &sub).await;
    let Some((inner, rest_at)) = parse_scan_head(&raw) else {
        return raw; // error passthrough
    };
    let next = if inner != 0 {
        (u64::from(target) << SCAN_CELL_SHIFT) | inner
    } else if target + 1 < shared.cells {
        u64::from(target + 1) << SCAN_CELL_SHIFT
    } else {
        0
    };
    let mut reply = shared.take_reply_buf();
    {
        let mut w = RespWriter::new(&mut reply, proto);
        w.array_header(2);
        let mut next_buf = [0u8; 20];
        w.bulk(crate::exec::fmt_u64(&mut next_buf, next));
    }
    reply.extend_from_slice(&raw[rest_at..]);
    shared.recycle_reply_buf(raw);
    reply
}

/// Scattered RANDOMKEY: two-level random — a random starting cell, then the
/// first non-empty cell in rotation answers (documented deviation).
async fn program_randomkey<O: PlaneObserver + 'static>(
    shared: &Rc<Shared<O>>,
    origin: ExecOrigin,
    proto: Protocol,
    id: u64,
    argv: &[&[u8]],
) -> Vec<u8> {
    let start = (crate::exec::next_rand(&shared.node) % u64::from(shared.cells)) as u16;
    for i in 0..shared.cells {
        let cell = CellId((start + i) % shared.cells);
        let raw = run_on(shared, origin, cell, proto, id, argv).await;
        if raw != b"$-1\r\n" && raw != b"_\r\n" {
            return raw;
        }
        shared.recycle_reply_buf(raw);
    }
    let mut reply = shared.take_reply_buf();
    RespWriter::new(&mut reply, proto).null();
    reply
}

/// `*N\r\n` array header → `(N, body offset)`. `None` for errors/nulls.
fn parse_array_header(raw: &[u8]) -> Option<(usize, usize)> {
    let rest = raw.strip_prefix(b"*")?;
    let nl = rest.windows(2).position(|w| w == b"\r\n")?;
    let n: i64 = core::str::from_utf8(&rest[..nl]).ok()?.parse().ok()?;
    if n < 0 {
        return None;
    }
    Some((n as usize, 1 + nl + 2))
}

/// `*2\r\n$N\r\n<cursor>\r\n…` SCAN reply head → `(cursor, keys offset)`.
fn parse_scan_head(raw: &[u8]) -> Option<(u64, usize)> {
    let rest = raw.strip_prefix(b"*2\r\n$")?;
    let nl = rest.windows(2).position(|w| w == b"\r\n")?;
    let len: usize = core::str::from_utf8(&rest[..nl]).ok()?.parse().ok()?;
    let start = nl + 2;
    let cursor = crate::exec::parse_cursor(rest.get(start..start + len)?)?;
    Some((cursor, 4 + 1 + start + len + 2))
}

/// `INF.TAKE`/`INF.PEEK` RESP2 reply: `*-1` ⇒ `Some(None)` (missing);
/// `*2 [$value][:pttl]` ⇒ value + pttl (−1 = no TTL).
fn parse_take_reply(raw: &[u8]) -> Option<Option<(Vec<u8>, i64)>> {
    if raw.starts_with(b"*-1\r\n") {
        return Some(None);
    }
    let rest = raw.strip_prefix(b"*2\r\n$")?;
    let nl = rest.windows(2).position(|w| w == b"\r\n")?;
    let len: usize = core::str::from_utf8(&rest[..nl]).ok()?.parse().ok()?;
    let start = nl + 2;
    let value = rest.get(start..start + len)?.to_vec();
    let tail = rest.get(start + len + 2..)?.strip_prefix(b":")?;
    let nl2 = tail.windows(2).position(|w| w == b"\r\n")?;
    let pttl: i64 = core::str::from_utf8(&tail[..nl2]).ok()?.parse().ok()?;
    Some(Some((value, pttl)))
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

/// Ship `argv` to `to` as an `Apply` and return the reply waiter, waiting
/// for fabric credits when exhausted (backpressure, never unbounded
/// queueing). The send time is queued for delivery-side RTT recording.
/// `Err` carries the refusal reply when the argv exceeds the codec's
/// argument cap.
async fn send_apply<O: PlaneObserver + 'static>(
    shared: &Rc<Shared<O>>,
    to: CellId,
    proto: Protocol,
    argv: &[&[u8]],
) -> Result<GateWait<u64, OwnedOutcome>, Vec<u8>> {
    let Some(args) = ApplyArgs::new(argv) else {
        let mut reply = Vec::new();
        RespWriter::new(&mut reply, proto).error("ERR too many arguments for cross-cell execution");
        return Err(reply);
    };
    // Routing is the `to` cell; the slot field is advisory (keyless scatter
    // applies carry the empty-key slot).
    let slot = SlotRouter::slot_of(argv.get(1).copied().unwrap_or(b""));
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
    shared.rtt_sent.borrow_mut()[usize::from(to.0)].push_back((token.0, shared.now.get()));
    Ok(waiter)
}
