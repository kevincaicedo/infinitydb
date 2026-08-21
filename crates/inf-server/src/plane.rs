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
use inf_fabric::{
    ApplyArgs, CellFabric, ErrCode, FabricToken, MAX_APPLY_ARGS, Op, Outcome, SendError,
};
use inf_foundation::time::Nanos;
use inf_foundation::{CellId, LogHistogram};
#[cfg(feature = "doc")]
use inf_log::DocLineage;
use inf_log::fs::{SegmentFs, StdSegmentFs};
use inf_log::{FsyncClass, MutationEffect, SegmentRotor};
use inf_runtime::GroupClass;
use inf_runtime::{
    CellPlane, Completion, CompletionResult, CompletionToken, FabricGate, GateWait, IoOp, LoopCx,
    RawFd, TokenClass, WaitList,
};
#[cfg(feature = "doc")]
use inf_store::JsonLogDecision;
use inf_store::{
    CellStore, EvictBudget, ExpiryBudget, Keyspace, LogFullImage, NsId, NsMode, SlotRouter,
    WallAnchor,
};
use inf_wire::{
    ArgvRef, CmdFlags, CommandId, ConnParser, Parsed, ParserLimits, Protocol, RespWriter, arity_ok,
    extract_keys, lookup,
};

mod tiered;

use crate::control::{ControlHandle, RecoveryBoard};
use crate::durable::{DurableCell, DurableConfig, EVERYSEC_TIMER_KEY};
#[cfg(feature = "doc")]
use crate::exec::DocLogAdmission;
use crate::exec::{ConnCx, NodeInfo, execute, execute_slices, stall_request};
use crate::pubsub::{self, PubSubCell, SubKind};
use crate::recover::{Recovery, RecoveryProgress};

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
/// Sized to the parked-reply working set — up to `conns × REMOTE_WINDOW`
/// `Bytes` gate values hold pool buffers at once on the natural-routing
/// leg; at 256 the pool exhausted and the overflow paid malloc/free per
/// reply (M2.5 Phase H allocator lever). Worst-case retention is
/// `REPLY_POOL_MAX × REPLY_POOL_BUF_CAP` = 16 MiB/cell, reached only if a
/// ≥ 4 KiB-reply workload actually held that many buffers concurrently
/// (the pool only keeps what the workload used; L5 note in the ledger).
const REPLY_POOL_MAX: usize = 4096;
const REPLY_POOL_BUF_CAP: usize = 4096;
/// Deferred-command pool bounds (`OwnedCmd` flat buffers), mirroring the
/// reply pool: the queue depth behind an active pump is bounded by
/// `PENDING_HIGH_WATER` per connection, and buffers recycle at dispatch.
const CMD_POOL_MAX: usize = 4096;
const CMD_POOL_BUF_CAP: usize = 4096;
/// Argv views for dispatch live on the stack up to this arity (M2.5
/// Phase H: `OwnedCmd::slices` was one heap `Vec` per dispatched command);
/// wider commands (MSET…) fall back to the heap.
const ARGV_INLINE: usize = 16;
/// Hard cap on wheel fires per expiry MAINTAIN slice — the debt-aware
/// escalation (M1-S05) may multiply the deficit budget, never exceed this.
const MAX_EXPIRY_FIRES_PER_SLICE: u32 = 4096;
/// Hard caps on one backfill MAINTAIN tick (M4.5-S05, ADR-0077 D3): the
/// deficit budget scales the slice, these bound its worst case — the
/// docs cap keeps one tick well under the 2 ms foreground co-gate at
/// the measured per-document walk cost.
#[cfg(feature = "doc")]
const MAX_BACKFILL_DOCS_PER_TICK: u32 = 1024;
#[cfg(feature = "doc")]
const MAX_BACKFILL_STEPS_PER_TICK: u32 = 8192;
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
    /// Flatten `argv` into `buf` (recycled through `Shared::cmd_pool` —
    /// M2.5 Phase H: `from_argv` was one malloc/free per deferred command,
    /// and on the natural-routing leg every remote command defers).
    fn from_argv_into(argv: &ArgvRef<'_>, mut buf: Vec<u8>) -> OwnedCmd {
        let argc = argv.len();
        let head = 4 + 4 * argc;
        let total = head + (0..argc).map(|i| argv.arg(i).len()).sum::<usize>();
        buf.clear();
        buf.reserve(total);
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

    /// Borrowed views over the flat buffer (`extract_keys`/`ApplyArgs`/
    /// observer want `&[&[u8]]`). Heap fallback for wide commands — the
    /// dispatch hot path uses the [`ARGV_INLINE`] stack array instead.
    fn slices(&self) -> Vec<&[u8]> {
        (0..self.argc()).map(|i| self.arg(i)).collect()
    }

    fn mem(&self) -> usize {
        self.buf.capacity()
    }

    /// Surrender the flat buffer for recycling (`Shared::recycle_cmd_buf`).
    fn into_buf(self) -> Vec<u8> {
        self.buf
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
    /// Injected-clock ms since the staged output first exceeded the pub/sub
    /// soft cap (M1-S11); 0 = under the soft limit.
    cob_soft_since_ms: u64,
    /// The output-cap kill was already requested (idempotent counter guard).
    cob_kill_sent: bool,
}

impl Conn {
    fn state_bytes(&self) -> usize {
        size_of::<Conn>()
            + self.parser.buffered()
            + self.out.capacity()
            + self.queue.iter().map(OwnedCmd::mem).sum::<usize>()
            + self.cx.sub_channels.iter().map(|c| c.len() + 24).sum::<usize>()
            + self.cx.sub_patterns.iter().map(|p| p.len() + 24).sum::<usize>()
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

struct Shared<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static> {
    cell: CellId,
    cells: u16,
    router: SlotRouter,
    /// Forces every key local — the cross-cell penalty A/B leg (§6 gate).
    route_local_only: bool,
    /// `DEBUG SLEEP` cell stall: connection parse/respond pause until this
    /// injected-clock instant (fabric service continues — deadlock safety).
    stall_until: Cell<Nanos>,
    store: RefCell<Keyspace>,
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
    /// Recycled `OwnedCmd` flat buffers (deferred commands) — one
    /// malloc/free per queued command otherwise; on the natural-routing
    /// leg every remote command defers (M2.5 Phase H). Bounded by
    /// [`CMD_POOL_MAX`]/[`CMD_POOL_BUF_CAP`].
    cmd_pool: RefCell<Vec<Vec<u8>>>,
    /// Running capacity sums of the two recycle pools (L5 — the
    /// `reply_pool_bytes`/`cmd_pool_bytes` gauges): maintained at the
    /// push/pop sites so the MAINTAIN flush never walks up to 4096
    /// buffers per pool (v0.4.0-alpha RSS-attribution instrument).
    reply_pool_bytes: Cell<u64>,
    cmd_pool_bytes: Cell<u64>,
    recv_dropped: Cell<u64>,
    /// Pub/sub registries (M1-S10): local subscriber lists, owner-side
    /// per-cell counts, the replicated pattern index.
    pubsub: RefCell<PubSubCell<ConnKey>>,
    /// Fabric-origin PUBLISHes awaiting this cell's owner pump (FIFO — the
    /// queue preserves per-publisher delivery order across fan-outs).
    pub_queue: RefCell<VecDeque<OwnerPub>>,
    pub_pump_active: Cell<bool>,
    /// Parsed `client-output-buffer-limit pubsub` `(hard, soft, soft_ms)`
    /// (M1-S11); refreshed by the MAINTAIN config sweep. Zeros disable.
    cob_pubsub: Cell<(u64, u64, u64)>,
    /// The durable plane (M2-S08, ADR-0015): `None` = memory-only cell —
    /// the zero-cost branch every memory-path check reduces to (M2-S09).
    durable: RefCell<Option<DurableCell<F>>>,
    /// The tiered plane half (M4-S26): flush pipelines, cold-read
    /// custody, MAINTAIN drivers. `None` until the durable plane exists
    /// (a tiered namespace is a configuration of `MODE durable` —
    /// ADR-0062 D1); inner state stays empty until one materializes.
    tier: RefCell<Option<crate::tier_cell::TierCell<F>>>,
    /// Fabric-origin tiered applies (M4-S26): per-origin FIFO queues. A
    /// tiered apply can suspend on a cold read, so it cannot run inside
    /// the synchronous FABRIC-IN drain — each origin's applies run on
    /// one FIFO pump future. Per-connection command order is preserved
    /// (the fabric delivers per-pair FIFO; the pump applies in arrival
    /// order); cross-origin applies interleave freely. Recorded bound:
    /// fabric-origin cold-read concurrency is `cells − 1` per owner.
    /// M4.5-S27 (ADR-0083 D1): flat durable-namespace applies join the
    /// same per-origin queue under staging pressure — pacing instead of
    /// the owner-side `-BUSY` refusal — and whenever the pump already
    /// holds this origin's work (FIFO = apply order). Bounded by the
    /// origins' fabric windows, never a new unbounded queue.
    ns_applies: RefCell<Vec<VecDeque<NsApply>>>,
    ns_pump_active: RefCell<Vec<bool>>,
    /// Gated `always` verdicts the apply pumps produced (M4.5-S29):
    /// each is a fabric reply awaiting this cell's fsync watermark. The
    /// pump queues the verdict and moves on — holding its FIFO across the
    /// durability wait serialized every fabric origin to one write per
    /// fsync (the S29 flat-scaling defect). FABRIC-IN drains this into
    /// the same deferred-reply futures the synchronous drain spawns
    /// (ADR-0015 D6 — the client-visible ack still never precedes this
    /// cell's fsync). Bounded by the origins' fabric windows.
    pump_gated: RefCell<VecDeque<GatedReply>>,
    /// Node control-thread handle (id allocation + catalog persistence).
    control: RefCell<Option<Arc<ControlHandle>>>,
    /// DDL pumps parked on catalog persistence (ADR-0015 D3).
    ddl_waiters: WaitList<u8>,
    /// `INF.CKPT WAIT` pumps parked on checkpoint-board publications
    /// (M2-S20 — the persist-epoch waitlist class).
    ckpt_waiters: WaitList<u8>,
    /// Last persist epoch MAINTAIN observed (edge-detects wakes).
    ddl_epoch_seen: Cell<u64>,
    /// Board published-sum at the last MAINTAIN (the ckpt-wake edge).
    ckpt_pub_seen: Cell<u64>,
    /// Node is loading (M2-S15): commands without the LOADING flag answer
    /// `-LOADING` until every cell's recovery completes. One predictable
    /// `Cell<bool>` load on the command path; false forever after boot.
    loading: Cell<bool>,
    /// Fabric-apply staged prefetch (M2.5 Phase H, ADR-0005 shape): FABRIC-IN
    /// stages drained applies, prefetches the whole batch's store lines, then
    /// executes. Off by default until its binding A/B (`--fabric-apply-prefetch`).
    apply_prefetch: Cell<bool>,
    /// Parse-batch staged prefetch (M2.5 Phase H, the ADR-0029 second lever —
    /// the same ADR-0005 shape on the client parse loop's local fast path):
    /// PARSE stages fast-path commands (flat copy + hash + probe-line
    /// prefetch), then executes the batch in parse order. Off by default
    /// until its binding A/B (`--parse-batch-prefetch`).
    parse_prefetch: Cell<bool>,
    /// De-async dispatch (M2.5 Phase H, ADR-0030 D4): the pump tries a
    /// synchronous fast path per command (single-owner remote `Apply`,
    /// local mirror) before constructing the `dispatch_one` future.
    /// Rejected by A/B (2026-07-10, ADR-0034): the machinery it removes
    /// measured ~2% of the natural mix — the L6 fast path was already
    /// near-zero-cost. Default off; kept as the A/B instrument for the
    /// S19 8-cell re-read (`--deasync-dispatch`).
    deasync_dispatch: Cell<bool>,
}

/// One fabric-origin namespace apply parked for its origin's FIFO pump
/// (M4-S26 tiered; M4.5-S27 added flat durable applies under staging
/// pressure — the suspension-capable sibling of the gated-reply future).
struct NsApply {
    token: FabricToken,
    ns: NsId,
    proto: Protocol,
    args: Vec<Vec<u8>>,
}

/// One fabric-origin PUBLISH parked at the owner cell.
struct OwnerPub {
    origin: CellId,
    token: FabricToken,
    channel: Vec<u8>,
    payload: Vec<u8>,
}

/// One armed maintenance bracket (M4.5-S04, ADR-0076 D3): the write-set
/// keys plus the optional mutation path the prune consumes. The `NsId`
/// variant carries the resolved numbered-db namespace.
#[cfg(feature = "doc")]
type ArmedDbBracket<'a> = (NsId, Vec<&'a [u8]>, Option<inf_doc::PathProgram>);
#[cfg(feature = "doc")]
type ArmedNsBracket<'a> = (Vec<&'a [u8]>, Option<inf_doc::PathProgram>);

impl<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static> Shared<O, F> {
    fn with_conn<R>(&self, key: ConnKey, f: impl FnOnce(&mut Conn) -> R) -> Option<R> {
        self.conns.borrow_mut().get_mut(key).map(f)
    }

    /// Executes owned argv locally (queued and remote-`Apply` paths),
    /// appending the reply to `out` (callers reuse scratch buffers — the
    /// owner side of a remote `Apply` is zero-allocation, M0-E8), and
    /// reports the apply point.
    #[allow(clippy::too_many_arguments)] // internal execution funnel
    fn execute_owned_into(
        &self,
        origin: ExecOrigin,
        argv: &[&[u8]],
        proto: Protocol,
        id: u64,
        db: u16,
        ns: Option<NsId>,
        out: &mut Vec<u8>,
    ) {
        let before = out.len();
        let mut cx = ConnCx {
            proto,
            id,
            db,
            ns,
            sub_channels: Vec::new(),
            sub_patterns: Vec::new(),
            node: Rc::clone(&self.node),
            close_requested: Cell::new(false),
        };
        let now = self.now.get();
        #[cfg(feature = "doc")]
        let capture_doc_log = lookup(argv[0])
            .is_some_and(|meta| crate::json::is_json_write(meta.id))
            && self.node.doc_log_admission.get().is_some();
        #[cfg(feature = "doc")]
        {
            if capture_doc_log {
                self.node.doc_log.borrow_mut().clear();
            } else {
                self.node.doc_log_admission.set(None);
            }
        }
        // Numbered-db maintenance bracket (M4.5-S04, ADR-0076 D3 row 3):
        // every numbered-db write funnels through this function, so one
        // attachment covers the mirror, the MSET legs, and the fabric
        // apply structurally. Named-ns calls (`Some(ns)`) are bracketed
        // at their two plane sites, where the commit half must follow
        // effect staging. Zero-index namespaces pay one guard branch.
        #[cfg(feature = "doc")]
        let bracket: Option<ArmedDbBracket<'_>> = if ns.is_none() {
            let target = NsId(u32::from(db));
            if self.store.borrow().ns_indexed(target) {
                match lookup(argv[0]) {
                    // COPY is store-mini-bracketed (ADR-0076 D3): its
                    // destination may live in another database.
                    Some(meta)
                        if meta.flags.contains(CmdFlags::WRITE) && meta.id != CommandId::Copy =>
                    {
                        let keys = extract_keys_slices(meta, argv);
                        if keys.is_empty() {
                            None
                        } else {
                            let path = self.json_mutation_path(meta.id, argv);
                            Some((target, keys, path))
                        }
                    }
                    _ => None,
                }
            } else {
                None
            }
        } else {
            None
        };
        #[cfg(feature = "doc")]
        if let Some((target, keys, path)) = &bracket
            && let Err(refusal) =
                self.store.borrow_mut().idx_bracket_begin(*target, keys, path.as_ref())
        {
            // Typed refusal before anything changed (ADR-0072 D7.1).
            RespWriter::new(out, proto).error(refusal.message());
            self.node.doc_log_admission.set(None);
            self.observer.borrow_mut().on_execute(self.cell, origin, argv, &out[before..], now);
            return;
        }
        execute_slices(argv, &mut self.store.borrow_mut(), &mut cx, now, out);
        #[cfg(feature = "doc")]
        if let Some((target, keys, _)) = &bracket {
            // Numbered dbs never stage — the commit half attaches at the
            // same boundary with the staging call absent (ADR-0072 D3).
            self.store.borrow_mut().idx_bracket_commit(*target, keys);
        }
        #[cfg(feature = "doc")]
        self.node.doc_log_admission.set(None);
        self.observer.borrow_mut().on_execute(self.cell, origin, argv, &out[before..], now);
    }

    /// The mutation's path program for the S04 static path-overlap prune
    /// (ADR-0076 D6): only unambiguous path-carrying JSON writes yield
    /// one — everything else evaluates in full. A path that fails to
    /// compile never prunes (the command will fail with its own error).
    #[cfg(feature = "doc")]
    fn json_mutation_path(&self, id: CommandId, argv: &[&[u8]]) -> Option<inf_doc::PathProgram> {
        let text: &[u8] = match id {
            CommandId::JsonSet if argv.len() >= 4 => argv[2],
            CommandId::JsonNumIncrBy | CommandId::JsonNumMultBy if argv.len() >= 4 => argv[2],
            CommandId::JsonDel | CommandId::JsonForget if argv.len() >= 3 => argv[2],
            CommandId::JsonToggle | CommandId::JsonClear | CommandId::JsonArrPop
                if argv.len() >= 3 =>
            {
                argv[2]
            }
            CommandId::JsonArrAppend | CommandId::JsonArrInsert | CommandId::JsonArrTrim
                if argv.len() >= 4 =>
            {
                argv[2]
            }
            CommandId::JsonMerge if argv.len() >= 4 => argv[2],
            // With three args the trailing one is the value (legacy root
            // path) — ambiguous positions never prune.
            CommandId::JsonStrAppend if argv.len() == 4 => argv[2],
            _ => return None,
        };
        let max_path_bytes = self.store.borrow().db(0)?.doc_max_path_bytes();
        let mut cache = self.node.path_cache.borrow_mut();
        cache.get_or_compile(text, max_path_bytes).ok().cloned()
    }

    /// An empty reply buffer, recycled when possible.
    fn take_reply_buf(&self) -> Vec<u8> {
        let mut buf = self.reply_pool.borrow_mut().pop().unwrap_or_default();
        let cap = buf.capacity() as u64;
        debug_assert!(self.reply_pool_bytes.get() >= cap, "pool byte sum tracks contents");
        self.reply_pool_bytes.set(self.reply_pool_bytes.get() - cap);
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
            self.reply_pool_bytes.set(self.reply_pool_bytes.get() + buf.capacity() as u64);
            pool.push(buf);
        }
    }

    /// An empty `OwnedCmd` flat buffer, recycled when possible.
    fn take_cmd_buf(&self) -> Vec<u8> {
        let buf = self.cmd_pool.borrow_mut().pop().unwrap_or_default();
        let cap = buf.capacity() as u64;
        debug_assert!(self.cmd_pool_bytes.get() >= cap, "pool byte sum tracks contents");
        self.cmd_pool_bytes.set(self.cmd_pool_bytes.get() - cap);
        buf
    }

    /// Returns an `OwnedCmd` buffer to the pool (bounded; oversized drop).
    fn recycle_cmd_buf(&self, buf: Vec<u8>) {
        if buf.capacity() == 0 || buf.capacity() > CMD_POOL_BUF_CAP {
            return;
        }
        let mut pool = self.cmd_pool.borrow_mut();
        if pool.len() < CMD_POOL_MAX {
            self.cmd_pool_bytes.set(self.cmd_pool_bytes.get() + buf.capacity() as u64);
            pool.push(buf);
        }
    }

    /// Typed single-key DEL/UNLINK/EXISTS/TOUCH apply (local or owner side):
    /// the reply is the integer count contribution; observer sees the
    /// synthesized single-key command with its `:N` reply.
    fn apply_counted(&self, origin: ExecOrigin, name: &[u8], key: &[u8], db: u16) -> i64 {
        let now = self.now.get();
        let del = name.eq_ignore_ascii_case(b"DEL") || name.eq_ignore_ascii_case(b"UNLINK");
        let hit = {
            let mut ks = self.store.borrow_mut();
            let store = ks.db_mut(usize::from(db));
            if del { store.del(key, now) } else { store.exists(key, now) }
        };
        let mut reply = Vec::new();
        RespWriter::new(&mut reply, Protocol::Resp2).int(i64::from(hit));
        self.observer.borrow_mut().on_execute(self.cell, origin, &[name, key], &reply, now);
        i64::from(hit)
    }

    /// Typed DBSIZE apply (scatter contribution, M1-S02; per selected db).
    fn apply_dbsize(&self, origin: ExecOrigin, db: u16) -> i64 {
        let now = self.now.get();
        let len = self.store.borrow_mut().db_mut(usize::from(db)).len() as i64;
        let mut reply = Vec::new();
        RespWriter::new(&mut reply, Protocol::Resp2).int(len);
        self.observer.borrow_mut().on_execute(self.cell, origin, &[b"DBSIZE"], &reply, now);
        len
    }

    // ---- durable-namespace execution (M2-S08, ADR-0015 D5/D6) ----

    /// The wall anchor for `ExpireAt` record conversion (L7-injected).
    fn wall_anchor(&self) -> WallAnchor {
        let (internal_ms, unix_ms) = self.node.wall_anchor.get();
        WallAnchor { internal_ms, unix_ms }
    }

    /// Conservative staging-bytes estimate for one command's effects:
    /// per write key, the key + the current post-image + record overhead,
    /// plus every argument byte (covers APPEND/SETRANGE growth). Checked
    /// *before* execution so a mutation is never applied unlogged.
    fn estimate_effect_bytes(
        &self,
        ns: NsId,
        meta: &'static inf_wire::CommandMeta,
        argv: &[&[u8]],
    ) -> usize {
        let ks = self.store.borrow();
        let now = self.now.get();
        let arg_bytes: usize = argv.iter().map(|a| a.len()).sum();
        // Canonical idoc can expand JSON text by at most the scalar-f64
        // case (9 bytes for a minimum 3-byte token); ×4 also covers the
        // fixed idoc header and container framing. Existing bytes are
        // added separately below. This is admission, so conservatism wins.
        #[cfg(feature = "doc")]
        let arg_reserve = if crate::json::is_json_write(meta.id) {
            arg_bytes.saturating_mul(4)
        } else {
            arg_bytes
        };
        #[cfg(not(feature = "doc"))]
        let arg_reserve = arg_bytes;
        let mut est = 64usize.saturating_add(arg_reserve);
        let store = ks.ns_store(ns);
        // Per-key reserve mirrors `stage_durable_effects` by effect class
        // (M4.5-S27, ADR-0083 D2). The estimate stays an upper bound —
        // `stage()` treats a post-admission refusal as an invariant
        // violation — but classes whose post-image provably excludes the
        // current image must not charge it: with the old blanket
        // `+image`, a DEL of a near-capacity value could never be
        // admitted (a park livelock), and a replace-SET was double-billed.
        for key in extract_keys_slices(meta, argv) {
            let per_key = match meta.id {
                // Delete effect only: `Delete { ns, key }` (+96 framing).
                CommandId::Del | CommandId::Unlink | CommandId::Getdel => key.len() + 96,
                // Replace class: the post-image is the new value, already
                // counted in `arg_reserve`; the second key term covers the
                // optional `ExpireAt` rider record.
                CommandId::Set
                | CommandId::Setnx
                | CommandId::Setex
                | CommandId::Psetex
                | CommandId::Getset
                | CommandId::Mset
                | CommandId::Msetnx => 2 * key.len() + 128,
                // SETRANGE builds `max(old_len, offset + payload)`: the
                // zero-padded gap is in no argument and not in the old
                // image — charge the declared offset too (a malformed
                // offset parses as 0 and fails in execution anyway).
                CommandId::Setrange => {
                    let offset: usize = argv
                        .get(2)
                        .and_then(|a| std::str::from_utf8(a).ok())
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(0);
                    let image = store.and_then(|s| s.log_image_bytes(key, now)).unwrap_or(0);
                    key.len() + 96 + image.max(offset)
                }
                // Read-modify default (APPEND, INCR-family, COPY, expiry
                // rewrites, doc writes): post ≤ current image + arguments.
                _ => {
                    let image = store.and_then(|s| s.log_image_bytes(key, now)).unwrap_or(0);
                    key.len() + 96 + image
                }
            };
            est = est.saturating_add(per_key);
        }
        est
    }

    #[cfg(feature = "doc")]
    #[allow(clippy::too_many_arguments)]
    fn stage_doc_full(
        cell: &mut DurableCell<F>,
        ns: NsId,
        key: &[u8],
        lineage: DocLineage,
        version: u32,
        idoc: &[u8],
        expire_at_ms: Option<u64>,
        class: FsyncClass,
        anchor: WallAnchor,
    ) -> u64 {
        let mut last =
            cell.stage(&MutationEffect::DocFull { ns, key, lineage, version, idoc }, class);
        if let Some(ms) = expire_at_ms {
            let at_unix_ms = anchor.unix_from_internal(Nanos::from_millis(ms));
            last = cell.stage(&MutationEffect::ExpireAt { ns, at_unix_ms, key }, class);
        }
        last
    }

    /// Post-execution effect emission (ADR-0015 D5): per written key, the
    /// post-image (+ `ExpireAt` when a deadline is set) or a `Delete` —
    /// the one hook covering every string/key/expiry command. Returns the
    /// last staged seq (the `always` gate key).
    fn stage_durable_effects(
        &self,
        ns: NsId,
        meta: &'static inf_wire::CommandMeta,
        argv: &[&[u8]],
        class: FsyncClass,
    ) -> Option<u64> {
        let mut ks = self.store.borrow_mut();
        let mut durable = self.durable.borrow_mut();
        let cell = durable.as_mut().expect("emission requires the durable plane");
        let store = ks.ns_store_mut(ns)?;
        let now = self.now.get();
        let anchor = self.wall_anchor();
        let mut last = None;

        #[cfg(feature = "doc")]
        if crate::json::is_json_write(meta.id) {
            let keys = extract_keys_slices(meta, argv);
            let key = *keys.first()?;
            let scratch = self.node.doc_log.borrow();
            match &scratch.intent {
                crate::json::DocLogIntent::None => {}
                crate::json::DocLogIntent::Delete => {
                    last = Some(cell.stage(&MutationEffect::Delete { ns, key }, class));
                }
                crate::json::DocLogIntent::Full => {
                    let decision = store
                        .json_log_full(key, now)
                        .expect("captured full key remains a live document until staging");
                    let JsonLogDecision::Full { lineage, version, idoc, expire_at_ms } = decision
                    else {
                        unreachable!("json_log_full always chooses a full image")
                    };
                    last = Some(Self::stage_doc_full(
                        cell,
                        ns,
                        key,
                        lineage,
                        version,
                        &idoc,
                        expire_at_ms,
                        class,
                        anchor,
                    ));
                }
                crate::json::DocLogIntent::Delta { program, opcode, match_count } => {
                    let candidate = MutationEffect::DocDelta {
                        ns,
                        key,
                        lineage: DocLineage::FIRST,
                        base_version: 0,
                        match_count: *match_count,
                        post_len: 1,
                        opcode: *opcode as u8,
                        program: program.as_bytes(),
                        operand: &scratch.operand,
                    };
                    let decision = store.json_log_delta_decision(
                        key,
                        candidate.encoded_len(),
                        scratch.operand.len(),
                        now,
                    );
                    match decision {
                        Some(JsonLogDecision::Delta { lineage, base_version, post_len }) => {
                            let effect = MutationEffect::DocDelta {
                                ns,
                                key,
                                lineage,
                                base_version,
                                match_count: *match_count,
                                post_len,
                                opcode: *opcode as u8,
                                program: program.as_bytes(),
                                operand: &scratch.operand,
                            };
                            last = Some(cell.stage(&effect, class));
                        }
                        Some(JsonLogDecision::Full { lineage, version, idoc, expire_at_ms }) => {
                            last = Some(Self::stage_doc_full(
                                cell,
                                ns,
                                key,
                                lineage,
                                version,
                                &idoc,
                                expire_at_ms,
                                class,
                                anchor,
                            ));
                        }
                        None => panic!("captured delta key remains a live document until staging"),
                    }
                }
            }
            return last;
        }

        let keys = extract_keys_slices(meta, argv);
        for key in &keys {
            match store.log_full_image(key, now) {
                #[cfg(feature = "doc")]
                Some(LogFullImage::JsonDoc(JsonLogDecision::Full {
                    lineage,
                    version,
                    idoc,
                    expire_at_ms,
                })) => {
                    last = Some(Self::stage_doc_full(
                        cell,
                        ns,
                        key,
                        lineage,
                        version,
                        &idoc,
                        expire_at_ms,
                        class,
                        anchor,
                    ));
                }
                #[cfg(feature = "doc")]
                Some(LogFullImage::JsonDoc(JsonLogDecision::Delta { .. })) => {
                    unreachable!("full-image probe never returns a delta")
                }
                Some(LogFullImage::String(img)) => {
                    let set = MutationEffect::StringSet { ns, key, value: img.value };
                    last = Some(cell.stage(&set, class));
                    if let Some(ms) = img.expire_at_ms {
                        let at_unix_ms = anchor.unix_from_internal(Nanos::from_millis(ms));
                        let exp = MutationEffect::ExpireAt { ns, at_unix_ms, key };
                        last = Some(cell.stage(&exp, class));
                    }
                }
                None => {
                    last = Some(cell.stage(&MutationEffect::Delete { ns, key }, class));
                }
            }
        }
        last
    }

    /// Owner-side named-namespace apply (the `ApplyNs` handler): admission,
    /// execution, emission — and for `always` writes the deferred-reply
    /// verdict (the fabric reply waits for this cell's fsync watermark).
    fn execute_ns_owned(
        &self,
        from: CellId,
        argv: &[&[u8]],
        proto: Protocol,
        ns: NsId,
        out: &mut Vec<u8>,
    ) -> NsApplyOutcome {
        let before = out.len();
        let meta = lookup(argv[0]);
        let class = self.store.borrow().ns_fsync_class(ns);
        let is_write = meta.is_some_and(|m| m.flags.contains(CmdFlags::WRITE));
        if let (Some(meta), Some(_)) = (meta, class)
            && is_write
        {
            match self.durable_admission(ns, meta, argv) {
                DurableAdmission::Admit => {}
                // Pacing, not refusal (M4.5-S27, ADR-0083 D1): the caller
                // parks this apply on the origin's FIFO pump; nothing was
                // executed or staged, so the retry re-enters here whole.
                DurableAdmission::Park => return NsApplyOutcome::Park,
                DurableAdmission::Refuse(refusal) => {
                    RespWriter::new(out, proto).error(refusal);
                    return NsApplyOutcome::Reply;
                }
            }
        }
        // Maintenance bracket, fabric named-ns row (ADR-0072 D3 /
        // ADR-0076 D3 row 1): pre-half after admission, before execute;
        // commit-half after effect staging, before the reply queues.
        #[cfg(feature = "doc")]
        let bracket: Option<ArmedNsBracket<'_>> = match meta {
            Some(meta)
                if is_write && meta.id != CommandId::Copy && self.store.borrow().ns_indexed(ns) =>
            {
                let keys = extract_keys_slices(meta, argv);
                if keys.is_empty() {
                    None
                } else {
                    let path = self.json_mutation_path(meta.id, argv);
                    Some((keys, path))
                }
            }
            _ => None,
        };
        #[cfg(feature = "doc")]
        if let Some((keys, path)) = &bracket
            && let Err(refusal) = self.store.borrow_mut().idx_bracket_begin(ns, keys, path.as_ref())
        {
            RespWriter::new(out, proto).error(refusal.message());
            return NsApplyOutcome::Reply;
        }
        self.execute_owned_into(ExecOrigin::Fabric(from), argv, proto, 0, 0, Some(ns), out);
        let mut outcome = NsApplyOutcome::Reply;
        if is_write
            && out.get(before) != Some(&b'-')
            && let (Some(meta), Some(class)) = (meta, class)
            && let Some(seq) = self.stage_durable_effects(ns, meta, argv, class)
            && class == FsyncClass::Always
        {
            self.durable.borrow_mut().as_mut().expect("staged above").note_gated_ack();
            outcome = NsApplyOutcome::Gated(seq);
        }
        #[cfg(feature = "doc")]
        if let Some((keys, _)) = &bracket {
            self.store.borrow_mut().idx_bracket_commit(ns, keys);
        }
        outcome
    }

    /// Durable-write admission — one typed verdict for every path
    /// (M4.5-S27, ADR-0083 D1/D2): the local pump and the fabric pump
    /// both park on [`DurableAdmission::Park`]; only conditions no drain
    /// can ever cure refuse. The pre-fix shape — local parks, fabric
    /// replies `-BUSY` — made hard refusal the node's dominant behaviour
    /// under pressure (~¾ of writes are fabric-routed at 4 cells).
    fn durable_admission(
        &self,
        ns: NsId,
        meta: &'static inf_wire::CommandMeta,
        argv: &[&[u8]],
    ) -> DurableAdmission {
        let durable = self.durable.borrow();
        let Some(cell) = durable.as_ref() else {
            return DurableAdmission::Refuse(
                "ERR durable namespace on a cell without durable storage",
            );
        };
        if cell.failed {
            return DurableAdmission::Refuse("ERR durable plane failed (fail-stop)");
        }
        if cell.space_exhausted() {
            return DurableAdmission::Refuse(
                "ERR durable write refused: log storage exhausted (NOSPACE)",
            );
        }
        drop(durable);
        let est = self.estimate_effect_bytes(ns, meta, argv);
        let durable = self.durable.borrow();
        let cell = durable.as_ref().expect("checked above");
        let record_max = cell.staging.max_record_len() as usize;
        if !cell.would_fit(est) {
            // `would_fit(est)` can never pass when `est > record_max`:
            // parking is then a livelock, not backpressure (the M2-S08
            // up-front bound check `staging.rs` demands — ADR-0083 D2).
            if est > record_max {
                #[cfg(feature = "doc")]
                if crate::json::is_json_write(meta.id) {
                    // The doc estimate is ×4-conservative; the exact
                    // late checks (`json.rs` record-max/budget) govern
                    // with real encoded bytes — admit to execution.
                    let (budget, record_max) = cell.staging_limits();
                    self.node.doc_log_admission.set(Some(DocLogAdmission { budget, record_max }));
                    return DurableAdmission::Admit;
                }
                self.node.log_admission_oversized.set(self.node.log_admission_oversized.get() + 1);
                return DurableAdmission::Refuse(crate::durable::STAGING_OVERSIZED_ERROR);
            }
            return DurableAdmission::Park;
        }
        #[cfg(feature = "doc")]
        if crate::json::is_json_write(meta.id) {
            let (budget, record_max) = cell.staging_limits();
            self.node.doc_log_admission.set(Some(DocLogAdmission { budget, record_max }));
        }
        DurableAdmission::Admit
    }
}

/// One durable-admission verdict (M4.5-S27, ADR-0083): `Park` is
/// backpressure a drain will cure (wake on `drained`); `Refuse` is a
/// typed condition no retry can cure and goes to the client.
enum DurableAdmission {
    Admit,
    Park,
    Refuse(&'static str),
}

/// One owner-side `always` reply deferred on the fsync watermark.
struct GatedReply {
    to: CellId,
    token: FabricToken,
    seq: u64,
    reply: Vec<u8>,
}

/// Owner-side verdict for one `ApplyNs`.
enum NsApplyOutcome {
    /// The reply is staged in the scratch range — ship it now.
    Reply,
    /// `always` write: ship the reply only once seq is durable.
    Gated(u64),
    /// Staging pressure (M4.5-S27, ADR-0083 D1): nothing executed or
    /// staged — the apply parks on the origin's FIFO pump and retries
    /// when the drain wakes it, instead of refusing with `-BUSY`.
    Park,
}

/// Applies the internal `INF.NSFAN` DDL fan on a peer cell (M2-S08):
/// `CREATE name mode fsync policy maxmemory id` / `DROP name`, fields as
/// the origin serialized them (`-` = none). Returns false for non-NSFAN.
fn handle_ns_apply<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Shared<O, F>,
    from: CellId,
    token: FabricToken,
    argv: &[&[u8]],
    scratch: &mut Vec<u8>,
    staged: &mut Vec<(CellId, FabricToken, StagedReply)>,
) -> bool {
    if !argv[0].eq_ignore_ascii_case(b"INF.NSFAN") {
        return false;
    }
    let start = scratch.len();
    let mut w = RespWriter::new(scratch, Protocol::Resp2);
    match apply_nsfan(shared, argv) {
        Ok(()) => w.simple("OK"),
        Err(e) => crate::admin::ns_error(e, &mut w),
    }
    staged.push((from, token, StagedReply::Bytes(start, scratch.len())));
    true
}

fn apply_nsfan<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Shared<O, F>,
    argv: &[&[u8]],
) -> Result<(), inf_store::NsError> {
    use inf_store::NsError;
    let malformed = NsError::InvalidName; // internal vocabulary; never client-visible
    if argv.len() == 3 && argv[1].eq_ignore_ascii_case(b"DROP") {
        return shared.store.borrow_mut().ns_drop(argv[2]);
    }
    if argv.len() == 4 && argv[1].eq_ignore_ascii_case(b"SET") {
        let tier = tier_from_fan(argv[3])?.ok_or(malformed)?;
        return shared.store.borrow_mut().ns_set_tier(argv[2], tier);
    }
    // Memory-namespace pressure keys (M4-S27, ADR-0068 D3): the MEMCFG
    // tag disambiguates from the tier-SET arm above.
    if argv.len() == 6 && argv[1].eq_ignore_ascii_case(b"SET") && argv[3] == b"MEMCFG" {
        let policy = match argv[4] {
            b"-" => None,
            p => Some(
                core::str::from_utf8(p)
                    .ok()
                    .and_then(inf_store::EvictionPolicy::parse)
                    .ok_or_else(|| malformed.clone())?,
            ),
        };
        let maxmemory = match argv[5] {
            b"-" => None,
            b => Some(
                core::str::from_utf8(b)
                    .ok()
                    .and_then(|v| v.parse::<u64>().ok())
                    .ok_or_else(|| malformed.clone())?,
            ),
        };
        return shared.store.borrow_mut().ns_set_memory(argv[2], policy, maxmemory);
    }
    if argv.len() != 9 || !argv[1].eq_ignore_ascii_case(b"CREATE") {
        return Err(malformed);
    }
    fn parse_str(b: &[u8]) -> Result<&str, inf_store::NsError> {
        core::str::from_utf8(b).map_err(|_| inf_store::NsError::InvalidName)
    }
    let mode = NsMode::parse(parse_str(argv[3])?).ok_or_else(|| malformed.clone())?;
    let fsync = match argv[4] {
        b"-" => None,
        b"everysec" => Some(FsyncClass::Everysec),
        b"always" => Some(FsyncClass::Always),
        _ => return Err(malformed),
    };
    let policy = match argv[5] {
        b"-" => None,
        p => {
            Some(inf_store::EvictionPolicy::parse(parse_str(p)?).ok_or_else(|| malformed.clone())?)
        }
    };
    let maxmemory = match argv[6] {
        b"-" => None,
        b => Some(parse_str(b)?.parse::<u64>().map_err(|_| malformed.clone())?),
    };
    let id: u32 = parse_str(argv[7])?.parse().map_err(|_| malformed.clone())?;
    let tier = tier_from_fan(argv[8])?;
    shared.store.borrow_mut().ns_create(inf_store::NsSpec {
        id: NsId(id),
        name: argv[2].to_vec(),
        mode,
        fsync,
        policy,
        maxmemory,
        tier,
    })
}

/// Serializes a tier spec for the `INF.NSFAN` vector (M4-S19): `-` for
/// absent, else ten colon-joined fields in ADR-0062 D2 table order. An
/// internal wire between symmetric binaries — the decoder still runs the
/// full range gauntlet (defense against a foreign or torn fan).
fn tier_to_fan(tier: Option<&inf_store::TierSpec>) -> Vec<u8> {
    let Some(t) = tier else { return b"-".to_vec() };
    let io = match t.tier_io_mode {
        inf_log::fs::TierIoMode::Buffered => "buffered",
        inf_log::fs::TierIoMode::Direct => "direct",
    };
    format!(
        "{}:{}:{}:{}:{}:{}:{}:{}:{}:{}",
        t.mem_budget_bytes,
        t.disk_budget_bytes,
        t.mutable_permille,
        t.maintain_slice_bytes,
        t.cold_read_qd,
        t.compaction_dead_ratio_pct,
        t.compaction_slice_bytes,
        t.blob_threshold_bytes,
        io,
        t.tail_stall_timeout_ms,
    )
    .into_bytes()
}

/// Decodes [`tier_to_fan`]'s encoding; `Ok(None)` for `-`.
fn tier_from_fan(bytes: &[u8]) -> Result<Option<inf_store::TierSpec>, inf_store::NsError> {
    use inf_store::NsError;
    if bytes == b"-" {
        return Ok(None);
    }
    let malformed = NsError::InvalidName; // internal vocabulary, as above
    let text = core::str::from_utf8(bytes).map_err(|_| malformed.clone())?;
    let fields: Vec<&str> = text.split(':').collect();
    if fields.len() != 10 {
        return Err(malformed);
    }
    let int = |s: &str| s.parse::<u64>().map_err(|_| malformed.clone());
    let tier = inf_store::TierSpec {
        mem_budget_bytes: int(fields[0])?,
        disk_budget_bytes: int(fields[1])?,
        mutable_permille: u32::try_from(int(fields[2])?).map_err(|_| malformed.clone())?,
        maintain_slice_bytes: int(fields[3])?,
        cold_read_qd: u16::try_from(int(fields[4])?).map_err(|_| malformed.clone())?,
        compaction_dead_ratio_pct: u8::try_from(int(fields[5])?).map_err(|_| malformed.clone())?,
        compaction_slice_bytes: int(fields[6])?,
        blob_threshold_bytes: u32::try_from(int(fields[7])?).map_err(|_| malformed.clone())?,
        tier_io_mode: match fields[8] {
            "buffered" => inf_log::fs::TierIoMode::Buffered,
            "direct" => inf_log::fs::TierIoMode::Direct,
            _ => return Err(malformed),
        },
        tail_stall_timeout_ms: u32::try_from(int(fields[9])?).map_err(|_| malformed.clone())?,
    };
    tier.validate().map_err(NsError::InvalidTierConfig)?;
    Ok(Some(tier))
}

/// One cell's data plane. Construct per cell, drive with
/// [`CellLoop::run_iteration`](inf_runtime::CellLoop::run_iteration).
pub struct ServerPlane<
    O: PlaneObserver + 'static = NoopObserver,
    F: SegmentFs + Clone + 'static = StdSegmentFs,
> {
    shared: Rc<Shared<O, F>>,
    listener: RawFd,
    started: bool,
    /// Recv completions staged from step 1 for PARSE+EXECUTE (step 3+4).
    inbox: Vec<(ConnKey, BufferId, u32)>,
    /// Reusable FABRIC-IN scratch: owner-side reply bytes for this drain.
    reply_scratch: Vec<u8>,
    /// Fabric-apply prefetch batch (reused across FABRIC-IN steps).
    apply_stage: Vec<StagedApply>,
    apply_stage_bytes: Vec<u8>,
    /// Parse-batch prefetch stage (M2.5 Phase H, ADR-0029 lever 2; reused
    /// across PARSE steps — one connection buffer at a time, flushed at
    /// every barrier).
    parse_stage: Vec<StagedParse>,
    parse_stage_bytes: Vec<u8>,
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
    /// Last CONFIG-store version pushed into the keyspace (M1-E3
    /// `hot-per-cell` sweep — one u64 compare per MAINTAIN, no re-parse).
    config_pushed: u64,
    /// The everysec wheel key was armed (M2-S05; once per plane).
    everysec_armed: bool,
    /// Last manual-checkpoint epoch observed on the control handle
    /// (M2-S10 — one relaxed load per MAINTAIN, edge-detected).
    ckpt_epoch_seen: u64,
    /// Early fabric publish at the head of MAINTAIN (M2.5-S21 lever,
    /// A/B-gated): remote ops staged during EXECUTE reach peers before
    /// this cell's MAINTAIN/LOG/RESPOND instead of at step 8.
    early_fabric_flush: bool,
    /// In-flight loop-resident boot recovery (M2-S15); `None` once this
    /// cell's log is recovered (or on memory-only cells).
    boot: Option<BootRecovery<F>>,
    /// Fatal boot-recovery error (§8.4): the assembly polls this after
    /// each iteration and fail-stops the process.
    boot_error: Option<std::io::Error>,
    /// Node recovery board while any cell is still loading (M2-S15);
    /// dropped once `all_ready` is observed.
    loading_board: Option<Arc<RecoveryBoard>>,
    /// MAINTAIN countdown for the memory-board publication (M3-S25): the
    /// keyspace-report walk is cheap but not free, so peers' `INFO`
    /// totals refresh on a coarse cadence instead of every iteration.
    memory_publish_in: u32,
}

/// One cell's loop-resident boot recovery (M2-S15): the [`Recovery`]
/// machine plus the config that seeds `enable_durable` on completion.
struct BootRecovery<F: SegmentFs> {
    rec: Recovery<F>,
    /// The filesystem tier, cloned into the ckpt/manifest drivers at
    /// `enable_durable` (M2-S19: `StdSegmentFs` on the node, `SimDisk`
    /// in the simulator).
    fs: F,
    cfg: DurableConfig,
    cell_id: u16,
    /// Earliest next step under the test-only throttle (`Nanos(0)` = now).
    next_due: Nanos,
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

impl<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static> ServerPlane<O, F> {
    /// `listener` must be a listening fd this plane's driver will own.
    #[allow(clippy::too_many_arguments)] // construction-time wiring, not an API surface
    pub fn new(
        cell: CellId,
        cells: u16,
        listener: RawFd,
        store: Keyspace,
        fabric: CellFabric,
        node: Rc<NodeInfo>,
        observer: O,
        route_local_only: bool,
    ) -> ServerPlane<O, F> {
        node.cell.set(cell.0);
        node.cells.set(cells);
        // M3-S10: size the per-cell program cache from the boot config
        // (`doc-path-cache-size` is BootOnly — assembly time IS its
        // application point).
        #[cfg(feature = "doc")]
        {
            let size = node
                .config
                .borrow()
                .get("doc-path-cache-size")
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(inf_doc::path::PROGRAM_CACHE_DEFAULT_ENTRIES);
            node.path_cache.replace(inf_doc::ProgramCache::new(size));
        }
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
                cmd_pool: RefCell::new(Vec::new()),
                reply_pool_bytes: Cell::new(0),
                cmd_pool_bytes: Cell::new(0),
                recv_dropped: Cell::new(0),
                pubsub: RefCell::new(PubSubCell::new(cells)),
                pub_queue: RefCell::new(VecDeque::new()),
                pub_pump_active: Cell::new(false),
                cob_pubsub: Cell::new((0, 0, 0)),
                durable: RefCell::new(None),
                tier: RefCell::new(None),
                ns_applies: RefCell::new((0..cells).map(|_| VecDeque::new()).collect::<Vec<_>>()),
                ns_pump_active: RefCell::new(vec![false; usize::from(cells)]),
                pump_gated: RefCell::new(VecDeque::new()),
                control: RefCell::new(None),
                ddl_waiters: WaitList::new(),
                ckpt_waiters: WaitList::new(),
                ddl_epoch_seen: Cell::new(0),
                ckpt_pub_seen: Cell::new(0),
                loading: Cell::new(false),
                apply_prefetch: Cell::new(false),
                parse_prefetch: Cell::new(false),
                deasync_dispatch: Cell::new(false),
            }),
            listener,
            started: false,
            inbox: Vec::new(),
            reply_scratch: Vec::new(),
            apply_stage: Vec::new(),
            apply_stage_bytes: Vec::new(),
            parse_stage: Vec::new(),
            parse_stage_bytes: Vec::new(),
            staged_replies: Vec::new(),
            park_flags: None,
            expiry_lag: 0,
            // MAX forces one push on the first MAINTAIN (boot-time config).
            config_pushed: u64::MAX,
            everysec_armed: false,
            ckpt_epoch_seen: 0,
            early_fabric_flush: false,
            boot: None,
            boot_error: None,
            loading_board: None,
            memory_publish_in: 1,
        }
    }

    /// Starts loop-resident boot recovery (M2-S15): the cell serves from
    /// its first iteration — answering `-LOADING` per the wire-layer gate —
    /// while MAINTAIN replays the cell log in bounded steps;
    /// [`enable_durable`](Self::enable_durable) fires internally on
    /// completion and the cell's board slot flips ready. Requires
    /// [`set_control`](Self::set_control) first (the board lives there).
    /// `now` is the injected clock instant recovery replays records under
    /// (one instant for the whole replay — L7, same contract as
    /// `open_cell_log`).
    ///
    /// # Panics
    /// If the control handle is not wired yet.
    pub fn begin_recovery(&mut self, fs: F, cfg: &DurableConfig, cell_id: u16, now: Nanos) {
        let board = {
            let control = self.shared.control.borrow();
            let control = control.as_ref().expect("set_control before begin_recovery");
            Arc::clone(control.recovery_board())
        };
        let anchor = self.shared.wall_anchor();
        self.boot = Some(BootRecovery {
            // Loop-resident boots defer boot-metadata fsyncs into driver
            // barriers (M2.5-S01): ready never blocks on the device.
            rec: Recovery::new(fs.clone(), cell_id, cfg, anchor, now).deferred_boot_sync(),
            fs,
            cfg: cfg.clone(),
            cell_id,
            next_due: Nanos(0),
        });
        self.loading_board = Some(board);
        self.shared.loading.set(true);
    }

    /// Fatal boot-recovery error, if one occurred (§8.4 fail-stop): the
    /// assembly polls this after each iteration and refuses to serve.
    pub fn take_boot_error(&mut self) -> Option<std::io::Error> {
        self.boot_error.take()
    }

    /// One bounded boot-recovery step (M2-S15), driven from MAINTAIN.
    /// Unthrottled boots replay flat-out (`before_park` keeps the loop
    /// polling); the test-only throttle paces steps against the loop
    /// clock, bounded below by the park timeout.
    fn drive_recovery(&mut self, cx: &mut LoopCx<'_>) {
        let Some(boot) = self.boot.as_mut() else { return };
        if cx.now < boot.next_due {
            return;
        }
        // Publish the phase BEFORE stepping (M2.5-S01): a step that stalls
        // inside the kernel leaves the board naming the stuck phase, so a
        // wedged cell is diagnosable from the control thread's narration
        // instead of silent (the ADR-0022 D7 signature).
        if let Some(board) = &self.loading_board {
            board.slot(boot.cell_id).publish_phase(boot.rec.phase_code());
        }
        let before = boot.rec.bytes_consumed();
        let step = {
            let mut store = self.shared.store.borrow_mut();
            boot.rec.step(&mut store, boot.cfg.recover.step_bytes)
        };
        // Pace on bytes actually read — never the slack credits progress
        // also carries (a near-empty prealloc'd segment must not charge
        // its extent to the throttle clock).
        let used = boot.rec.bytes_consumed() - before;
        if used > 0 {
            if let Some(rate) = boot.cfg.recover.throttle_bytes_per_sec {
                let delay = used.saturating_mul(1_000_000_000) / rate.max(1);
                boot.next_due = Nanos(cx.now.0.saturating_add(delay));
            }
            // Charged, never budget-gated: boot replay outranks the idle
            // maintenance classes (foreground is `-LOADING`-only here).
            cx.charge(GroupClass::Maintenance, u32::try_from(used / 1024).unwrap_or(u32::MAX));
        }
        if let Some(board) = &self.loading_board {
            let (segs_done, segs_total) = boot.rec.segments_progress();
            board.slot(boot.cell_id).publish(
                boot.rec.bytes_done(),
                boot.rec.bytes_total(),
                segs_done,
                segs_total,
            );
        }
        match step {
            Ok(RecoveryProgress::Working) => {}
            Ok(RecoveryProgress::Complete) => {
                let mut boot = self.boot.take().expect("boot present");
                let fs = boot.fs;
                let barrier_dirs = boot.rec.take_boot_barrier_dirs();
                let recovered_tiers = boot.rec.take_recovered_tiers();
                let (rotor, stats, seed) = boot.rec.finish();
                match self.enable_durable(fs, &boot.cfg, boot.cell_id, rotor, seed) {
                    Ok(()) => {
                        // Recovered tiered namespaces' plane half
                        // (M4-S26; ADR-0057 D6): the flush pipeline
                        // resumes with the manifested catalog and the
                        // cold-read table inherits the boot-opened fds.
                        if !recovered_tiers.is_empty() {
                            let ks = self.shared.store.borrow();
                            let mut tier = self.shared.tier.borrow_mut();
                            let tier = tier.as_mut().expect("enable_durable built tier state");
                            for rt in recovered_tiers {
                                let spec = ks
                                    .ns_get_by_id(rt.ns)
                                    .and_then(|spec| spec.tier)
                                    .expect("recovered namespace carries a tier block");
                                tier.install_recovered(rt.ns, &spec, rt.flush, rt.files);
                            }
                        }
                        // Boot-metadata durability rides driver barriers at
                        // the head of the commit ledger (M2.5-S01): every
                        // durable ack is fenced behind them by the
                        // done-prefix rule; ready flips now, without
                        // waiting on the device.
                        if let Some(cell) = self.shared.durable.borrow_mut().as_mut() {
                            cell.arm_boot_barriers(cx, barrier_dirs);
                        }
                        if let Some(board) = &self.loading_board {
                            let slot = board.slot(boot.cell_id);
                            slot.mark_ready(
                                stats.ckpt_records + stats.records_applied,
                                stats.torn_truncated_at,
                            );
                        }
                    }
                    Err(err) => self.boot_error = Some(err),
                }
            }
            Err(err) => {
                self.boot = None;
                self.boot_error = Some(err);
            }
        }
    }

    /// Enables the durable plane for this cell (M2-S08/S10/S11): the
    /// assembly runs recovery first (`recover::open_cell_log`) and hands
    /// the tail-opened rotor plus the recovered manifest here, before the
    /// loop starts. `cell_id` names the shard directory the checkpoint and
    /// manifest drivers write under.
    ///
    /// # Errors
    /// Checkpoint-directory scan failure (boot-time listing).
    pub fn enable_durable(
        &mut self,
        fs: F,
        cfg: &DurableConfig,
        cell_id: u16,
        rotor: SegmentRotor<F>,
        recovered: Option<crate::RecoveredManifest>,
    ) -> std::io::Result<()> {
        let shard_dir = cfg.data_dir.join(format!("shard-{cell_id}"));
        let ckpt_dir = shard_dir.join("ckpt");
        let ckpt = crate::ckpt::CkptCell::new(fs.clone(), ckpt_dir.clone(), cell_id, cfg.ckpt)?;
        *self.shared.tier.borrow_mut() = Some(crate::tier_cell::TierCell::new(
            fs.clone(),
            u32::from(cell_id),
            shard_dir.clone(),
        ));
        let manifest = crate::ckpt::ManifestCell::new(
            fs,
            shard_dir,
            ckpt_dir,
            cell_id,
            recovered.map(|m| crate::ckpt::PendingManifest {
                ckpt_id: m.ckpt_id,
                begin_lsn: m.begin_lsn,
            }),
        );
        *self.shared.durable.borrow_mut() = Some(DurableCell::new(
            cfg.staging,
            cfg.sync_pipeline,
            cfg.fua_p50_us_probed,
            rotor,
            ckpt,
            manifest,
        ));
        Ok(())
    }

    /// The tiered MAINTAIN half (M4-S26): reconcile the namespace set,
    /// run the per-namespace drivers (demote → flush → release,
    /// admission cadence, extent reclaim, retirement unlink), spawn
    /// compaction read chains, tear down dropped namespaces, and drain
    /// queued cold-read intents into `IoOp::TierRead`s — once per
    /// reactor iteration (the S10 admission rule). Zero-cost when no
    /// tiered namespace was ever created: one `None` check.
    fn tier_maintain(&mut self, cx: &mut LoopCx<'_>) {
        let mut compact_reads: Vec<crate::tier_cell::CompactRead> = Vec::new();
        let mut flush_ops: Vec<IoOp> = Vec::new();
        let cold = {
            let mut tier_slot = self.shared.tier.borrow_mut();
            let Some(tier) = tier_slot.as_mut() else { return };
            tier.sync_namespaces(&self.shared.store.borrow());
            if tier.namespaces.is_empty() && tier.cold.is_none() {
                return;
            }
            let (durable_mark, transition_idle) = {
                let durable = self.shared.durable.borrow();
                let mark = durable
                    .as_ref()
                    .map(|cell| (cell.stats().records_appended, cell.ack_gate.watermark()));
                let idle = durable.as_ref().is_some_and(DurableCell::ckpt_transition_idle);
                (mark, idle)
            };
            let mut ks = self.shared.store.borrow_mut();
            let mut units = 0u32;
            let mut fatal: Option<String> = None;
            let now_us = cx.now.as_micros();
            for at in 0..tier.namespaces.len() {
                match tier.maintain_ns(
                    &mut ks,
                    at,
                    durable_mark,
                    transition_idle,
                    now_us,
                    &mut flush_ops,
                ) {
                    Ok((used, work)) => {
                        units += used;
                        compact_reads.extend(work);
                    }
                    // Tier fsync failure is fatal-by-default (§8.4,
                    // ADR-0056 D4); typed I/O refusals already latched
                    // the device-full state inside the flush slice.
                    Err(err) if err.is_fatal() => {
                        fatal = Some(err.to_string());
                        break;
                    }
                    Err(_) => {}
                }
            }
            units += tier.maintain_teardown();
            if units > 0 {
                cx.charge(GroupClass::Maintenance, units);
            }
            if let Some(detail) = fatal {
                drop(ks);
                let mut durable = self.shared.durable.borrow_mut();
                let cell = durable.as_mut().expect("tiered namespaces require the durable plane");
                cell.fail_stop("tier flush", &detail);
            }
            // Reactor-drive flush observables (M4.5-S31, ADR-0084 D6).
            let stats = &tier.flush_stats;
            let files_sealed: u64 =
                tier.namespaces.iter().map(|t| t.flush.sealed().len() as u64).sum();
            self.shared.node.tier_flush.set([
                stats.rounds,
                stats.write_retries,
                stats.stale_completions,
                stats.round_us.percentile(50.0),
                stats.round_us.percentile(99.0),
                tier.flush_rounds_inflight(),
                files_sealed,
                tier.namespaces.iter().filter(|t| t.flush.active().is_some()).count() as u64,
            ]);
            tier.cold.clone()
        };
        // Reactor-drive flush ops (M4.5-S31): pushed outside the tier
        // borrow — REAP routes their completions back via
        // `on_flush_completion`.
        for op in flush_ops {
            cx.push(op);
        }
        for read in compact_reads {
            let shared = Rc::clone(&self.shared);
            let _ = cx.executor.poll_immediate(compact_pump(shared, read));
        }
        // Extent-seal fdatasyncs (ADR-0061 D3): the barrier is already
        // in the ledger; the op rides the driver now.
        let pending_syncs = self
            .shared
            .tier
            .borrow_mut()
            .as_mut()
            .map(crate::tier_cell::TierCell::take_pending_syncs)
            .unwrap_or_default();
        for (fd, ticket) in pending_syncs {
            cx.push(IoOp::Fdatasync { fd, token: crate::durable::fsync_token(ticket) });
        }
        if let Some(cold) = cold {
            cold.drain(|op| cx.push(op));
            // The ADR-0064 D3 split scrape + ADR-0055 cold counters,
            // flushed into per-cell gauges (`INFO tiering` renders them;
            // the worst cell binds harness-side).
            let tier = self.shared.tier.borrow();
            let tier = tier.as_ref().expect("cold engine implies tier state");
            let counters = cold.counters();
            // ADR-0055 D5: `1 − device/logical` — 0 at zero coalescing
            // (the v0.4.0-alpha soak rendered the inverted
            // `enqueued/issued`, 1000 exactly there — instrument fix).
            let coalesce_milli = counters.coalesce_ratio_milli();
            self.shared.node.cold_pool_bytes.set(cold.pool_reserved_bytes());
            self.shared.node.tiering_split.set([
                tier.ram_hit_us.percentile(50.0),
                tier.ram_hit_us.percentile(99.0),
                tier.ram_hit_us.percentile(99.9),
                tier.cold_us.percentile(50.0),
                tier.cold_us.percentile(99.0),
                tier.cold_us.percentile(99.9),
                cold.qd_percentile(99.0),
                coalesce_milli,
                cold.inflight_total() as u64,
                cold.queue_depth() as u64,
                cold.latency_percentile_us(99.0),
                counters.issued,
                counters.enqueued,
                counters.pool_dry,
                counters.queue_full,
            ]);
        }
    }

    /// Checkpoint gauges for tests/stats (`None` = memory-only cell).
    pub fn ckpt_stats(&self) -> Option<crate::ckpt::CkptStats> {
        self.shared.durable.borrow().as_ref().map(DurableCell::ckpt_stats)
    }

    /// Manifest/truncation gauges for tests/stats (`None` = memory-only
    /// cell — M2-S11).
    pub fn manifest_stats(&self) -> Option<crate::ckpt::ManifestStats> {
        self.shared.durable.borrow().as_ref().map(DurableCell::manifest_stats)
    }

    /// Wires the node control-thread handle (DDL id allocation + catalog
    /// persistence — ADR-0015 D2/D3). Also hands `INFO` the node-wide
    /// memory board (M3-S25 attribution fix).
    pub fn set_control(&mut self, control: Arc<ControlHandle>) {
        *self.shared.node.memory_board.borrow_mut() = Some(Arc::clone(control.memory_board()));
        *self.shared.control.borrow_mut() = Some(control);
    }

    /// Durable counters for tests/stats (`None` = memory-only cell).
    pub fn durable_stats(&self) -> Option<crate::durable::DurableStats> {
        self.shared.durable.borrow().as_ref().map(DurableCell::stats)
    }

    /// Wires this plane's slot of the doorbell-wakeup park board (the same
    /// `Arc` goes to every cell's fabric via `CellFabric::set_wakeups`).
    pub fn set_park_flags(&mut self, flags: Arc<Vec<AtomicBool>>) {
        self.park_flags = Some(flags);
    }

    /// Enables the M2.5-S21 early fabric publish (A/B lever): remote ops
    /// staged during EXECUTE are published at the head of MAINTAIN.
    pub fn set_early_fabric_flush(&mut self, on: bool) {
        self.early_fabric_flush = on;
    }

    /// Enables the M2.5 Phase-H fabric-apply staged prefetch (A/B lever,
    /// ADR-0005 shape): FABRIC-IN stages drained applies, prefetches the
    /// batch's store lines, then executes in arrival order.
    pub fn set_fabric_apply_prefetch(&mut self, on: bool) {
        self.shared.apply_prefetch.set(on);
    }

    /// Parse-batch staged prefetch (M2.5 Phase H, ADR-0029 lever 2): the
    /// parse loop stages local fast-path commands, prefetches the batch's
    /// store lines, then executes in parse order.
    pub fn set_parse_batch_prefetch(&mut self, on: bool) {
        self.shared.parse_prefetch.set(on);
    }

    /// De-async dispatch (M2.5 Phase H, ADR-0030 D4 lever): the pump
    /// attempts a synchronous fast path per command before falling back to
    /// the async `dispatch_one`.
    pub fn set_deasync_dispatch(&mut self, on: bool) {
        self.shared.deasync_dispatch.set(on);
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

    /// Memory attribution for this cell's keyspace slice (sim accounting
    /// oracle, tooling — never the data plane).
    pub fn keyspace_report(&self) -> inf_store::MemoryReport {
        self.shared.store.borrow().report()
    }

    /// Read-only keyspace access for DST oracles (M3-S23, ADR-0045 D5):
    /// the equivalence oracle compares live state against an independent
    /// log replay. Borrowed only between scheduler iterations — never
    /// across one (the suspension custody rule binds the harness too).
    pub fn keyspace(&self) -> core::cell::Ref<'_, Keyspace> {
        self.shared.store.borrow()
    }

    /// Pub/sub registry gauges `(owned channels, patterns, state bytes)` —
    /// the sim teardown oracle asserts all three return to zero once every
    /// subscriber unwound (M1-S15).
    pub fn pubsub_gauges(&self) -> (u64, u64, usize) {
        let ps = self.shared.pubsub.borrow();
        (ps.live_owned_channel_count(), ps.live_pattern_count(), ps.state_bytes())
    }

    /// Reaps every wheel entry already expired at `now`, ignoring slice
    /// budgets. Sim accounting oracle only: equalizes active-vs-lazy expiry
    /// between the node (wheel slices ran) and the replay model (none did)
    /// before live-record counts are compared.
    pub fn drain_expiry(&self, now: Nanos) -> u64 {
        let mut reaped = 0;
        loop {
            let stats = self
                .shared
                .store
                .borrow_mut()
                .expire_tick(now, ExpiryBudget { max_fires: u32::MAX, max_steps: u32::MAX });
            reaped += stats.reaped;
            if stats.reaped == 0 && stats.stale == 0 {
                return reaped;
            }
        }
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
        let Some(meta) = lookup(argv.arg(0)) else { return false };
        if !arity_ok(meta, argv.len()) {
            return false;
        }
        // Pub/sub always defers to the pump (even single-cell, even under
        // route_local_only): registries and delivery are plane state, and
        // subscriber registration must reach the owner cell before the
        // confirmation frame is emitted (M1-S10).
        if pubsub::is_plane_pubsub(meta.id) {
            return true;
        }
        // INF.NS always dispatches on the pump (M2-S08): CREATE/DROP run
        // the DDL program (id allocation + catalog persist) even on a
        // 1-cell node; USE is a conn-state barrier there.
        if meta.id == CommandId::InfNs {
            return true;
        }
        // INF.CKPT/BGSAVE/LASTSAVE ride the pump (M2-S20): they speak to
        // the control handle (request epochs, the checkpoint board).
        if matches!(meta.id, CommandId::InfCkpt | CommandId::Bgsave | CommandId::Lastsave) {
            return true;
        }
        if self.shared.route_local_only {
            return false;
        }
        let sub = (argv.len() > 1).then(|| argv.arg(1));
        if self.shared.cells > 1 && is_scatter(meta.id, sub) {
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

impl<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static> CellPlane for ServerPlane<O, F> {
    fn on_completion(&mut self, cx: &mut LoopCx<'_>, c: Completion) {
        match c.result {
            CompletionResult::Accepted { fd } => {
                let key = self.shared.conns.borrow_mut().insert(Conn {
                    fd,
                    parser: ConnParser::new(ParserLimits::default()),
                    cx: ConnCx {
                        proto: Protocol::Resp2,
                        id: 0,
                        db: 0,
                        ns: None,
                        sub_channels: Vec::new(),
                        sub_patterns: Vec::new(),
                        node: Rc::clone(&self.shared.node),
                        close_requested: Cell::new(false),
                    },
                    out: Vec::new(),
                    send_inflight: false,
                    closing: false,
                    close_after_flush: false,
                    pump_active: false,
                    queue: VecDeque::new(),
                    recv_disarmed: false,
                    rearm_recv: false,
                    cob_soft_since_ms: 0,
                    cob_kill_sent: false,
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
                let removed = self.shared.conns.borrow_mut().remove(key);
                let id = (u64::from(key.slot) << 32) | u64::from(key.generation);
                self.shared.node.clients.borrow_mut().unregister(id);
                // Pub/sub cleanup (M1-S10): drop the connection from the
                // local registries; 1→0 transitions notify the channel
                // owners / every cell (patterns) off the close path.
                if let Some(conn) = removed {
                    let notes = unsubscribe_closed_conn(&self.shared, key, &conn.cx);
                    if !notes.is_empty() {
                        let shared = Rc::clone(&self.shared);
                        let _ = cx.executor.poll_immediate(flush_sub_deltas(shared, notes));
                    }
                }
            }
            CompletionResult::Error { buf, errno } => {
                if let Some(buf) = buf {
                    cx.pool.release(buf);
                }
                // Tier-flush op failures (M4.5-S31, ADR-0084 D4) are the
                // round's to classify at the next MAINTAIN — ENOSPC
                // latches admission and retries; a failed barrier is the
                // §8.4 fatal class there. Never connection housekeeping.
                if matches!(c.token.class(), TokenClass::TierFlushWrite | TokenClass::TierFlushSync)
                {
                    let mut tier = self.shared.tier.borrow_mut();
                    let tier = tier.as_mut().expect("tier-flush completion implies tier state");
                    tier.on_flush_completion(c.token, Some(errno));
                    return;
                }
                // Log-op failures are fail-stop territory (§8.4), never
                // connection housekeeping. A zero-fill write failing is
                // the same class: the device refused a log write.
                if matches!(
                    c.token.class(),
                    TokenClass::LogWrite | TokenClass::Fsync | TokenClass::ZeroFillWrite
                ) {
                    let mut durable = self.shared.durable.borrow_mut();
                    let cell = durable.as_mut().expect("log completion without durable plane");
                    cell.on_log_error(c.token, errno);
                }
                // Checkpoint-op failures abort the checkpoint, never the
                // process (ADR-0016: the old checkpoint + log stay valid);
                // the token is not a connection.
                if matches!(c.token.class(), TokenClass::CkptWrite | TokenClass::CkptSync) {
                    let mut durable = self.shared.durable.borrow_mut();
                    let cell = durable.as_mut().expect("ckpt completion without durable plane");
                    cell.on_ckpt_error(errno);
                    return;
                }
                // MANIFEST-swap barrier failure: old recovery unit kept
                // (M2-S11, the checkpoint-abort class — ADR-0017).
                if c.token.class() == TokenClass::ManifestSync {
                    let mut durable = self.shared.durable.borrow_mut();
                    let cell = durable.as_mut().expect("manifest completion without durable plane");
                    cell.on_manifest_error(errno);
                    return;
                }
                // Cold-read failure (M4-S26): custody must release —
                // waiters observe the typed errno and the command layer
                // answers a typed error, never a crash (operating error).
                if c.token.class() == TokenClass::TierRead {
                    let tier = self.shared.tier.borrow();
                    let cold = tier
                        .as_ref()
                        .and_then(|t| t.cold.as_ref())
                        .expect("TierRead completion implies a live cold-read engine");
                    cold.on_completion(
                        c.token,
                        CompletionResult::Error { buf: None, errno },
                        cx.now.as_micros(),
                    );
                    return;
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
            // Log + checkpoint file ops (M2-S05/S08/S10): routed by token
            // class into the durable cell — lease release on a write's
            // terminal completion; watermark advance + gated-ack wakes on
            // a log Synced; checkpoint progress on the ckpt classes.
            CompletionResult::LogWritten => {
                // Tier-flush writes (M4.5-S31): a round-counter update in
                // the tier plane — never the WAL frame lease's custody.
                if c.token.class() == TokenClass::TierFlushWrite {
                    let mut tier = self.shared.tier.borrow_mut();
                    let tier = tier.as_mut().expect("tier-flush completion implies tier state");
                    tier.on_flush_completion(c.token, None);
                    return;
                }
                let mut durable = self.shared.durable.borrow_mut();
                let cell = durable.as_mut().expect("LogWritten without durable plane");
                match c.token.class() {
                    TokenClass::CkptWrite => cell.on_ckpt_written(),
                    // ADR-0086 D4: a zero slice landed on the next
                    // segment — the rotor's cursor, never the frame lease.
                    TokenClass::ZeroFillWrite => cell.on_zero_fill_written(),
                    _ => cell.on_log_written(cx),
                }
            }
            CompletionResult::Synced => {
                // Tier-flush barriers (M4.5-S31): recorded here, applied
                // at MAINTAIN — flush watermarks never ride the WAL
                // commit ledger (ADR-0084 D1).
                if c.token.class() == TokenClass::TierFlushSync {
                    let mut tier = self.shared.tier.borrow_mut();
                    let tier = tier.as_mut().expect("tier-flush completion implies tier state");
                    tier.on_flush_completion(c.token, None);
                    return;
                }
                let mut durable = self.shared.durable.borrow_mut();
                let cell = durable.as_mut().expect("Synced without durable plane");
                if c.token.class() == TokenClass::CkptSync {
                    cell.on_ckpt_synced();
                } else if c.token.class() == TokenClass::ManifestSync {
                    cell.on_manifest_synced();
                } else {
                    cell.on_synced(cx, c.token);
                }
            }
            CompletionResult::TierRead => {
                // M4-S26: route into the custody table — waiters wake and
                // their futures run in this iteration's EXECUTE slice.
                let tier = self.shared.tier.borrow();
                let cold = tier
                    .as_ref()
                    .and_then(|t| t.cold.as_ref())
                    .expect("TierRead completion implies a live cold-read engine");
                cold.on_completion(c.token, CompletionResult::TierRead, cx.now.as_micros());
            }
        }
    }

    fn on_timer(&mut self, cx: &mut LoopCx<'_>, key: u64) {
        if key == EVERYSEC_TIMER_KEY
            && let Some(cell) = self.shared.durable.borrow_mut().as_mut()
        {
            cell.on_everysec_tick(cx);
        }
    }

    fn before_park(&mut self) -> bool {
        // Boot replay pending: keep polling (M2-S15) — parking would gate
        // recovery throughput on the park timeout. Throttled (test-only)
        // boots do park; the park timeout bounds their step cadence.
        if let Some(boot) = &self.boot
            && boot.cfg.recover.throttle_bytes_per_sec.is_none()
        {
            return true;
        }
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
        // With `--fabric-apply-prefetch` (M2.5 Phase H), applies instead
        // stage (one small copy) so the whole pack's store lines prefetch
        // before any op executes — the ADR-0005 pipeline on the batch the
        // fabric naturally provides; order per source pair is unchanged.
        self.reply_scratch.clear();
        self.staged_replies.clear();
        let apply_prefetch = self.shared.apply_prefetch.get();
        let shared = &self.shared;
        let scratch = &mut self.reply_scratch;
        let staged = &mut self.staged_replies;
        let stage = &mut self.apply_stage;
        let stage_bytes = &mut self.apply_stage_bytes;
        let mut orphans: u64 = 0;
        let mut pubs: Vec<OwnerPub> = Vec::new();
        let mut gated: Vec<GatedReply> = Vec::new();
        let now = cx.now;
        let drained = shared.fabric.borrow_mut().drain(FABRIC_DRAIN_MAX, |from, op| {
            if apply_prefetch {
                stage_or_handle(
                    shared,
                    now,
                    from,
                    op,
                    stage,
                    stage_bytes,
                    scratch,
                    staged,
                    &mut pubs,
                    &mut gated,
                    &mut orphans,
                );
            } else {
                handle_fabric_op(
                    shared,
                    now,
                    from,
                    op,
                    scratch,
                    staged,
                    &mut pubs,
                    &mut gated,
                    &mut orphans,
                );
            }
        });
        flush_apply_stage(shared, stage, stage_bytes, scratch, staged, &mut pubs);
        // M4.5-S29: gated verdicts the tiered apply pumps queued since the
        // last pass join this pass's deferred-reply spawns below. They must
        // drain even on a fabric-quiet iteration — their producers ran in
        // run_ready, not in this drain.
        gated.extend(self.shared.pump_gated.borrow_mut().drain(..));
        if drained == 0 && gated.is_empty() {
            return;
        }
        if drained > 0 {
            cx.note_fabric(drained as u64);
        }

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
        drop(fabric);
        // Owner-side `always` applies (M2-S08, ADR-0015 D6): the fabric
        // reply itself is deferred — a future awaits this cell's ack gate,
        // then publishes the reply (flushed by the next FABRIC-OUT). The
        // client-visible ack never precedes the owning cell's fsync.
        for g in gated {
            let shared = Rc::clone(&self.shared);
            let _ = cx.executor.poll_immediate(async move {
                let waiter = {
                    let durable = shared.durable.borrow();
                    durable
                        .as_ref()
                        .expect("gated reply implies durable plane")
                        .ack_gate
                        .waiter(g.seq)
                };
                waiter.await;
                shared.fabric.borrow_mut().reply(g.to, g.token, &Outcome::Bytes(&g.reply));
            });
        }
        // Tiered fabric applies (M4-S26): wake each origin's FIFO pump.
        let tier_pending: Vec<u16> = {
            let queues = self.shared.ns_applies.borrow();
            let active = self.shared.ns_pump_active.borrow();
            (0..queues.len())
                .filter(|&i| !queues[i].is_empty() && !active[i])
                .map(|i| i as u16)
                .collect()
        };
        for origin in tier_pending {
            self.shared.ns_pump_active.borrow_mut()[usize::from(origin)] = true;
            let shared = Rc::clone(&self.shared);
            let _ = cx.executor.poll_immediate(ns_apply_pump(shared, origin));
        }
        // Fabric-origin PUBLISHes fan out on this cell's owner pump (one
        // long-lived FIFO future — arrival order is delivery order). The
        // reply to each publisher ships when its fan acks return.
        if !pubs.is_empty() {
            self.shared.pub_queue.borrow_mut().extend(pubs);
            if !self.shared.pub_pump_active.get() {
                self.shared.pub_pump_active.set(true);
                let shared = Rc::clone(&self.shared);
                let _ = cx.executor.poll_immediate(owner_pub_pump(shared));
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
        if !self.everysec_armed && self.shared.durable.borrow().is_some() {
            self.everysec_armed = true;
            cx.timers.insert(cx.now + Nanos::from_secs(1), EVERYSEC_TIMER_KEY);
        }
        self.shared.now.set(cx.now);
        // DEBUG SLEEP stall: connection processing pauses (inbox buffers
        // hold; pool pressure degrades to RecvDropped, never blocks the
        // thread); FABRIC-IN keeps serving peers.
        if cx.now < self.shared.stall_until.get() {
            return;
        }

        let stage_enabled = self.shared.parse_prefetch.get();
        let mut stage = core::mem::take(&mut self.parse_stage);
        let mut stage_bytes = core::mem::take(&mut self.parse_stage_bytes);
        let inbox = core::mem::take(&mut self.inbox);
        for (key, buf, len) in inbox {
            let mut commands: u32 = 0;
            // First command that must defer to a pump (everything after it
            // defers too — replies are ordered per connection).
            let mut deferred: Vec<OwnedCmd> = Vec::new();
            let mut spawn_first: Option<OwnedCmd> = None;
            let mut protocol_error = false;
            let mut quit = false;
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
                let origin = ExecOrigin::Conn(key.slot, key.generation);
                let mut iter = parser.feed(data);
                while let Some(parsed) = iter.next() {
                    match parsed {
                        Parsed::Command(argv) | Parsed::Inline(argv) => {
                            commands += 1;
                            let meta = lookup(argv.arg(0));
                            // M2-S15 `-LOADING` gate: while the node loads,
                            // only LOADING-flagged commands run; the rest
                            // answer exactly Redis's error (oracle capture
                            // artifact). Unknown commands keep their normal
                            // error — Redis resolves the command first. The
                            // board re-check serves commands that arrive in
                            // the same iteration the last cell flips ready.
                            if self.shared.loading.get()
                                && meta.is_some_and(|meta| !meta.flags.contains(CmdFlags::LOADING))
                            {
                                if self
                                    .loading_board
                                    .as_deref()
                                    .is_some_and(RecoveryBoard::all_ready)
                                {
                                    self.shared.loading.set(false);
                                } else {
                                    // The error reply keeps pipeline order:
                                    // staged commands answer first.
                                    flush_parse_stage(
                                        &self.shared,
                                        &mut stage,
                                        &mut stage_bytes,
                                        origin,
                                        conn_cx,
                                        out,
                                    );
                                    let mut w = RespWriter::new(out, conn_cx.proto);
                                    w.error("LOADING Redis is loading the dataset in memory");
                                    continue;
                                }
                            }
                            // Named-namespace commands always ride the pump
                            // (M2-S08): durable acks suspend, staging
                            // admission may park, and emission lives there —
                            // one `Option` load on the memory fast path.
                            let defer = pump_was_active
                                || spawn_first.is_some()
                                || !deferred.is_empty()
                                || conn_cx.ns.is_some()
                                || self.needs_fabric(&argv);
                            if defer {
                                // Pump replies emit after the fast-path
                                // replies already in `out` — flush so the
                                // staged batch keeps its pipeline position.
                                flush_parse_stage(
                                    &self.shared,
                                    &mut stage,
                                    &mut stage_bytes,
                                    origin,
                                    conn_cx,
                                    out,
                                );
                                let owned =
                                    OwnedCmd::from_argv_into(&argv, self.shared.take_cmd_buf());
                                if pump_was_active || spawn_first.is_some() {
                                    deferred.push(owned);
                                } else {
                                    spawn_first = Some(owned);
                                }
                            } else if stage_enabled && parse_stageable(meta, &argv) {
                                // M2.5 Phase H (ADR-0029 lever 2): stage the
                                // fast-path command — flat argv copy + key
                                // hash + phase-1 index-line prefetch; the
                                // batch executes at the next barrier or end
                                // of buffer with its record lines prefetched.
                                let (hash, has_key) = match argv.len() >= 2 {
                                    true => {
                                        let hash = CellStore::hash_key(argv.arg(1));
                                        if let Some(store) =
                                            self.shared.store.borrow().db(usize::from(conn_cx.db))
                                        {
                                            store.prefetch(hash);
                                        }
                                        (hash, true)
                                    }
                                    false => (0, false),
                                };
                                let off = stage_argv_block_argv(&mut stage_bytes, &argv);
                                stage.push(StagedParse { off, hash, has_key });
                            } else {
                                flush_parse_stage(
                                    &self.shared,
                                    &mut stage,
                                    &mut stage_bytes,
                                    origin,
                                    conn_cx,
                                    out,
                                );
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
                                    origin,
                                    &argv_slices,
                                    &out[before..],
                                    now,
                                );
                                if let Some(dur) = stall_request(&argv_slices) {
                                    self.shared.stall_until.set(now.saturating_add(dur));
                                }
                                // QUIT: stop processing this buffer (Redis
                                // discards anything pipelined after QUIT) and
                                // close once the +OK reply has flushed.
                                if conn_cx.close_requested.get() {
                                    conn_cx.close_requested.set(false);
                                    quit = true;
                                    break;
                                }
                            }
                        }
                        Parsed::Incomplete => {}
                        Parsed::ProtocolError(e) => {
                            flush_parse_stage(
                                &self.shared,
                                &mut stage,
                                &mut stage_bytes,
                                origin,
                                conn_cx,
                                out,
                            );
                            let mut w = RespWriter::new(out, conn_cx.proto);
                            w.error(&format!("ERR Protocol error: {e:?}"));
                            protocol_error = true;
                            break;
                        }
                    }
                }
                // End of buffer: the staged batch executes with its record
                // lines prefetched (§7.3 phase 2 across the whole batch).
                flush_parse_stage(&self.shared, &mut stage, &mut stage_bytes, origin, conn_cx, out);
                drop(iter);
                let conn = conns.get_mut(key).expect("conn checked above");
                if protocol_error || quit {
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
        debug_assert!(stage.is_empty(), "parse stage flushed before buffer release");
        self.parse_stage = stage;
        self.parse_stage_bytes = stage_bytes;
    }

    fn maintain(&mut self, cx: &mut LoopCx<'_>) {
        self.shared.now.set(cx.now);
        // ---- early fabric publish (M2.5-S21): remote ops staged during
        // EXECUTE become peer-visible NOW instead of at FABRIC-OUT (step
        // 8) — the peer drains them while this cell runs MAINTAIN/LOG/
        // RESPOND, so the hop RTT overlaps local work instead of
        // following it (the request-path sibling of the M0-R1 "publish
        // replies NOW" finding; remote throughput is window/RTT-bound at
        // REMOTE_WINDOW, so RTT saved converts directly). One extra
        // Release store + doorbell per destination per iteration; step 8
        // stays for late stagers.
        if self.early_fabric_flush {
            let mut fabric = self.shared.fabric.borrow_mut();
            let published = fabric.flush();
            if published > 0 {
                cx.note_fabric(published as u64);
            }
        }
        // ---- boot recovery (M2-S15): budgeted replay steps while the
        // cell answers -LOADING; enable_durable fires on completion.
        if self.boot.is_some() {
            self.drive_recovery(cx);
        }
        // ---- node memory board (M3-S25): publish this cell's gauges on
        // a coarse cadence so every cell's INFO renders fresh node-wide
        // totals (the serving cell re-publishes its own slot at render
        // time; this keeps the *peer* slots current).
        self.memory_publish_in -= 1;
        if self.memory_publish_in == 0 {
            self.memory_publish_in = 64;
            let node = &self.shared.node;
            if let Some(board) = node.memory_board.borrow().as_ref() {
                #[cfg_attr(not(feature = "doc"), allow(unused_mut))]
                let mut report = self.shared.store.borrow().report();
                #[cfg(feature = "doc")]
                node.add_cell_doc_memory(&mut report);
                board
                    .slot(self.shared.cell.0)
                    .publish(crate::exec::memory_gauges_of(&report, node));
            }
        }
        if let Some(board) = &self.loading_board {
            let node = &self.shared.node;
            let (done, total) = board.bytes();
            node.loading_start_unix_ms.set(board.start_unix_ms());
            node.loading_loaded_bytes.set(done);
            node.loading_total_bytes.set(total);
            node.loading_cells_ready.set(board.ready_cells());
            if board.all_ready() {
                node.loading.set(0);
                self.shared.loading.set(false);
                self.loading_board = None;
            } else {
                node.loading.set(1);
            }
        }
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
        // ---- index backfill (M4.5-S05, ADR-0077): budgeted walk slices
        // on the Maintenance class, then readiness publication and the D6
        // catalog flip. Guarded on registry emptiness (one Vec-len load
        // when the feature is unused) and on recovery completion — replay
        // maintains nothing by default (ADR-0076 D7), so a walk over a
        // half-replayed store would go stale silently.
        #[cfg(feature = "doc")]
        if self.boot.is_none() {
            let mut store = self.shared.store.borrow_mut();
            if !store.idx_registry().is_empty() {
                let budget = cx.budget(GroupClass::Maintenance);
                if budget > 0 {
                    let slice = inf_store::BackfillBudget {
                        max_docs: budget.saturating_mul(4).min(MAX_BACKFILL_DOCS_PER_TICK),
                        max_steps: budget.saturating_mul(32).min(MAX_BACKFILL_STEPS_PER_TICK),
                    };
                    let stats = store.idx_backfill_tick(cx.now, slice);
                    let units = (stats.docs_scanned + stats.reaped).min(u64::from(u32::MAX)) as u32
                        + stats.steps / 64;
                    if units > 0 {
                        cx.charge(GroupClass::Maintenance, units);
                    }
                }
                if let Some(control) = self.shared.control.borrow().as_ref() {
                    let board = control.index_board();
                    let cell = self.shared.cell.0;
                    // Republish every tick (ADR-0077 D4): ≤ 64 relaxed
                    // stores, and it makes the D5 rank rule self-healing.
                    for (slot, generation) in store.idx_ready_reports() {
                        board.publish_ready(cell, slot, generation);
                    }
                    let mut flipped = false;
                    for (id, slot, generation) in store.idx_fleet_candidates() {
                        if board.fleet_ready(slot, generation) {
                            // The ADR-0077 D6 flip: monotone, per cell,
                            // generation-exact.
                            store
                                .idx_registry_mut()
                                .set_catalog_state(id, inf_store::IndexState::Ready)
                                .expect("Backfilling → Ready is an ADR-0075 D3 edge");
                            flipped = true;
                        }
                    }
                    // One writer persists the flip (cell 0) — the durable
                    // `ready` is the ADR-0075 D4 rebuild-class hint S06's
                    // sidecar load reads at the next boot.
                    if flipped && cell == 0 {
                        control.request_persist(store.export_catalog(
                            control.next_ns_id(),
                            control.next_index_id(),
                            control.next_index_generation(),
                        ));
                    }
                }
            }
        }
        // ---- CLIENT KILL sweep: ids encode {slot:32 | generation:32}, so
        // the registry mark maps straight back to the conn slab.
        let kills = self.shared.node.clients.borrow_mut().take_kill_requests();
        for id in kills {
            let key = ConnKey { slot: (id >> 32) as u32, generation: id as u32 };
            self.initiate_close(cx, key);
        }
        // ---- pressure config push (M1-E3, hot-per-cell within one MAINTAIN
        // round): one u64 version compare per iteration; a real push only
        // when CONFIG SET (or boot wiring) touched the store.
        let config_version = self.shared.node.config.borrow().version();
        if config_version != self.config_pushed {
            self.config_pushed = config_version;
            crate::admin::push_pressure(&mut self.shared.store.borrow_mut(), &self.shared.node);
            // Output-cap config rides the same hot-per-cell sweep (M1-S11).
            self.shared
                .cob_pubsub
                .set(crate::config::pubsub_output_limit(&self.shared.node.config.borrow()));
        }
        // ---- eviction slice (M1-S06/S07): budgeted clock/CMS sweep toward
        // the low watermark + CMS decay. A no-op without a configured limit
        // (the cached-flag refresh keeps the write path one branch).
        let evict_budget = cx.budget(GroupClass::Maintenance);
        if evict_budget > 0 {
            let stats = self
                .shared
                .store
                .borrow_mut()
                .evict_tick(cx.now, EvictBudget { max_evictions: evict_budget });
            let units = (stats.evicted + stats.scanned_slots / 64).min(u64::from(u32::MAX)) as u32;
            if units > 0 {
                cx.charge(GroupClass::Maintenance, units);
            }
        }
        // ---- durable plane (M2-S08): segment prealloc rides MAINTAIN
        // (rotation stays a pointer swap — S02); durable counters flush
        // into NodeInfo for INFO persistence (S21 vocabulary).
        if let Some(cell) = self.shared.durable.borrow_mut().as_mut() {
            cell.maintain(cx);
            // Manual checkpoint requests ride the control handle (one
            // relaxed load — the persisted-epoch pattern, ADR-0016 D7).
            if let Some(control) = self.shared.control.borrow().as_ref() {
                let epoch = control.ckpt_board().slot(self.shared.cell.0).req();
                if epoch != self.ckpt_epoch_seen {
                    self.ckpt_epoch_seen = epoch;
                    cell.request_ckpt(epoch);
                }
            }
            // ---- checkpoint slice (M2-S10, ADR-0016 D5): its own deficit
            // class — a 10 GB walk can't starve expiry, and vice versa.
            let ckpt_budget = cx.budget(GroupClass::Checkpoint);
            if ckpt_budget > 0 {
                let anchor = self.shared.wall_anchor();
                let mut tier = self.shared.tier.borrow_mut();
                let used = cell.ckpt_slice(
                    &mut self.shared.store.borrow_mut(),
                    tier.as_mut(),
                    cx,
                    ckpt_budget,
                    anchor,
                );
                drop(tier);
                if used > 0 {
                    cx.charge(GroupClass::Checkpoint, used);
                }
            }
            // ---- MANIFEST + truncation slice (M2-S11/S12, ADR-0017):
            // watermark-gated swap machine (barriers ride the driver),
            // bounded segment forgets (unlinks delegated to the control
            // thread), orphan GC — charged to Maintenance.
            let control = self.shared.control.borrow();
            let unix_now_ms = self.shared.wall_anchor().unix_from_internal(cx.now);
            let mut tier = self.shared.tier.borrow_mut();
            let manifest_units = cell.manifest_slice(
                cx,
                control.as_deref(),
                unix_now_ms,
                &mut self.shared.store.borrow_mut(),
                tier.as_mut(),
            );
            drop(tier);
            drop(control);
            if manifest_units > 0 {
                cx.charge(GroupClass::Maintenance, manifest_units);
            }
            let stats = cell.stats();
            let node = &self.shared.node;
            node.log_records_appended.set(stats.records_appended);
            node.log_pending_bytes.set(stats.pending_log_bytes);
            node.log_last_durable_lsn.set(stats.last_durable_lsn);
            node.log_watermark_lag.set(stats.watermark_lag_lsn);
            node.log_fsyncs_completed.set(stats.fsyncs_completed);
            node.log_acks_gated.set(stats.acks_gated);
            node.log_frames_queued.set(stats.frames_queued);
            node.log_staging_bytes.set(stats.staging_resident_bytes);
            node.manifests_published.set(stats.manifests_published);
            node.manifests_aborted.set(stats.manifests_aborted);
            node.ckpt_in_progress.set(stats.ckpt_in_progress);
            node.segments_truncated.set(stats.segments_truncated);
            node.fsyncs_per_sec.set(stats.fsyncs_per_sec);
            node.acks_per_sec.set(stats.acks_per_sec);
            node.fsync_p50_us.set(stats.fsync_p50_us);
            node.fsync_p99_us.set(stats.fsync_p99_us);
            node.fsync_p999_us.set(stats.fsync_p999_us);
            node.fsync_group_p50.set(stats.fsync_group_p50);
            node.fsync_group_p99.set(stats.fsync_group_p99);
            node.log_write_stall_p50_us.set(stats.write_stall_p50_us);
            node.log_write_stall_p99_us.set(stats.write_stall_p99_us);
            node.log_write_stall_p999_us.set(stats.write_stall_p999_us);
            node.log_staging_capacity.set(stats.staging_capacity_bytes);
            node.log_admission_parked.set(stats.admission_parked);
            node.log_admission_parked_total.set(stats.admission_parked_total);
            node.fsyncs_linked.set(stats.fsyncs_linked);
            node.fsyncs_seal.set(stats.fsyncs_seal);
            node.fsyncs_standalone.set(stats.fsyncs_standalone);
            node.barrier_class_fua.set(stats.barrier_class_fua);
            node.fsyncs_fua.set(stats.fsyncs_fua);
            node.fua_p50_us.set(stats.fua_p50_us);
            node.fua_p99_us.set(stats.fua_p99_us);
            node.log_padding_bytes.set(stats.log_padding_bytes);
            node.zero_fill_bytes.set(stats.zero_fill_bytes);
            node.rotations_unzeroed.set(stats.rotations_unzeroed);
            node.rotations_upgrade.set(stats.rotations_upgrade);
            node.barrier_class_degraded.set(stats.barrier_class_degraded);
            node.fsyncs_completion.set(stats.fsyncs_completion);
            node.log_segments_live.set(stats.log_segments_live);
            let ckpt = cell.ckpt_stats();
            let unix_now_ms = self.shared.wall_anchor().unix_from_internal(cx.now);
            node.ckpt_age_s.set(if ckpt.last_unix_ms == 0 {
                0
            } else {
                unix_now_ms.saturating_sub(ckpt.last_unix_ms) / 1000
            });
            node.ckpts_completed.set(ckpt.completed);
            node.ckpts_aborted.set(ckpt.aborted);
            node.ckpt_last_unix_ms.set(ckpt.last_unix_ms);
            node.ckpt_last_begin_lsn.set(ckpt.last_begin_lsn);
            node.ckpt_buffer_bytes.set(ckpt.buffer_bytes);
        }
        // ---- tiered plane half (M4-S26): namespace sync, the four
        // drivers, the cold-read drain (once per iteration — S10).
        self.tier_maintain(cx);
        // ---- DDL persist wakes (ADR-0015 D3): one relaxed load per
        // MAINTAIN; parked DDL pumps wake on epoch edges.
        if let Some(control) = self.shared.control.borrow().as_ref() {
            let epoch = control.persisted_epoch();
            if epoch != self.shared.ddl_epoch_seen.get() {
                self.shared.ddl_epoch_seen.set(epoch);
                self.shared.ddl_waiters.wake_all(0);
            }
            // ---- checkpoint-publication wakes (M2-S20): any cell's
            // durable MANIFEST changes the board sum; parked INF.CKPT
            // WAIT pumps re-check their target. Also the LASTSAVE gauge.
            let board = control.ckpt_board();
            let sum = board.published_sum();
            if sum != self.shared.ckpt_pub_seen.get() {
                self.shared.ckpt_pub_seen.set(sum);
                self.shared.ckpt_waiters.wake_all(0);
            }
            self.shared.node.rdb_last_save_ms.set(board.max_unix_ms());
        }
        // ---- stats flush
        let node = &self.shared.node;
        node.recv_dropped.set(self.shared.recv_dropped.get());
        node.fabric_rtt_p50_ns.set(self.shared.rtt_ns.borrow().percentile(50.0));
        {
            let ps = self.shared.pubsub.borrow();
            node.pubsub_channels.set(ps.live_owned_channel_count());
            node.pubsub_patterns.set(ps.live_pattern_count());
            node.pubsub_state_bytes.set(ps.state_bytes() as u64);
        }
        let caps = self.shared.cob_pubsub.get();
        let mut conns = self.shared.conns.borrow_mut();
        node.connections.set(conns.live as u64);
        let mut bytes = 0usize;
        for conn in conns.slots.iter_mut().flatten() {
            bytes += conn.state_bytes();
            // Soft-cap aging continues between deliveries (M1-S11): a
            // stalled subscriber over the soft limit dies on schedule even
            // when no further message arrives.
            if conn.cob_soft_since_ms != 0 {
                enforce_output_cap(node, conn, cx.now.as_millis(), caps);
            }
        }
        node.conn_state_bytes.set(bytes as u64);
        // Recycle-pool residency (v0.4.0-alpha RSS-attribution gauges):
        // running sums maintained at the push/pop sites, flushed here.
        node.reply_pool_bytes.set(self.shared.reply_pool_bytes.get());
        node.cmd_pool_bytes.set(self.shared.cmd_pool_bytes.get());
    }

    fn seal_log(&mut self, cx: &mut LoopCx<'_>) {
        if let Some(cell) = self.shared.durable.borrow_mut().as_mut() {
            cell.seal_log(cx);
        }
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
#[allow(clippy::too_many_arguments)] // the FABRIC-IN drain context, not an API surface
fn handle_fabric_op<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
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
        Op::Apply { token, cmd, args, .. } => {
            handle_apply(shared, from, token, cmd, args.as_slice(), scratch, staged, pubs);
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
        Op::ApplyNs { token, cmd, ns, args, .. } => {
            // Named-namespace apply (M2-S08, ADR-0015 D1): the namespace
            // travels as an explicit id; the owner resolves class and
            // semantics authoritatively (never trusting the origin).
            let argv = args.as_slice();
            let proto = if cmd & 0x0F == 3 { Protocol::Resp3 } else { Protocol::Resp2 };
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
                });
                return;
            }
            let start = scratch.len();
            match shared.execute_ns_owned(from, argv, proto, NsId(ns), scratch) {
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
                    });
                }
            }
        }
        Op::Batch { ops } => {
            for nested in ops {
                handle_fabric_op(shared, now, from, nested, scratch, staged, pubs, gated, orphans);
            }
        }
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
    argv: &[&[u8]],
    scratch: &mut Vec<u8>,
    staged: &mut Vec<(CellId, FabricToken, StagedReply)>,
    pubs: &mut Vec<OwnerPub>,
) {
    {
        {
            // Internal pub/sub fabric vocabulary (M1-S10) — intercepted
            // ahead of `execute`, so it needs no registry entries and stays
            // invisible to clients (an `INF.PUBFAN` typed by a client is an
            // unknown command). One first-byte gate keys the comparisons.
            if argv[0].first().is_some_and(|b| b | 0x20 == b'i') {
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
struct StagedApply {
    from: CellId,
    token: FabricToken,
    cmd: u8,
    /// Offset of the flat argv block in the stage scratch.
    off: u32,
    /// `CellStore::hash_key(argv[1])` when the op carries a key argument.
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
/// [`stage_argv_block`] (argc ≤ [`MAX_APPLY_ARGS`] by codec construction).
fn read_argv_block<'b>(bytes: &'b [u8], off: u32, out: &mut [&'b [u8]; MAX_APPLY_ARGS]) -> usize {
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

/// One local fast-path command staged by the parse-batch prefetch (M2.5
/// Phase H, ADR-0029 lever 2 — the ADR-0005 pipeline shape on the batch the
/// parse loop naturally provides): argv flat-copied into the stage scratch,
/// key hash computed (and index probe lines prefetched) at stage time.
/// Execution reads `ConnCx` live at flush, so stage-time state is only ever
/// a prefetch hint — never an execution input (conn-state mutators are
/// flush barriers besides).
struct StagedParse {
    /// Offset of the flat argv block in the stage scratch.
    off: u32,
    /// `CellStore::hash_key(argv[1])` when the command carries a key.
    hash: u64,
    has_key: bool,
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
fn parse_stageable(meta: Option<&'static inf_wire::CommandMeta>, argv: &ArgvRef<'_>) -> bool {
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
fn stage_argv_block_argv(bytes: &mut Vec<u8>, argv: &ArgvRef<'_>) -> u32 {
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
fn flush_parse_stage<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
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
        execute(argv, &mut shared.store.borrow_mut(), conn_cx, now, out);
        shared.observer.borrow_mut().on_execute(shared.cell, origin, argv, &out[before..], now);
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
/// reply order per source pair are exactly the inline path's.
#[allow(clippy::too_many_arguments)] // the FABRIC-IN drain context, not an API surface
fn stage_or_handle<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
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
        Op::Apply { token, cmd, args, .. } => {
            let argv = args.as_slice();
            let db = u16::from(cmd >> 4);
            let (hash, has_key) = match argv.get(1) {
                Some(key) => {
                    let hash = CellStore::hash_key(key);
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
            stage.push(StagedApply { from, token, cmd, off, hash, db, has_key });
        }
        Op::Batch { ops } => {
            for nested in ops {
                stage_or_handle(
                    shared,
                    now,
                    from,
                    nested,
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
        other => {
            flush_apply_stage(shared, stage, stage_bytes, scratch, staged, pubs);
            handle_fabric_op(shared, now, from, other, scratch, staged, pubs, gated, orphans);
        }
    }
}

/// Execute the staged applies: one record-line pass and one document-root
/// pass over the whole batch (§7.3 + ADR-0044 — dependent misses overlap
/// across the batch instead of serializing per op), then execution in
/// arrival order.
fn flush_apply_stage<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
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
    let mut argv_buf: [&[u8]; MAX_APPLY_ARGS] = [b""; MAX_APPLY_ARGS];
    for e in stage.iter() {
        let argc = read_argv_block(stage_bytes, e.off, &mut argv_buf);
        handle_apply(shared, e.from, e.token, e.cmd, &argv_buf[..argc], scratch, staged, pubs);
    }
    stage.clear();
    stage_bytes.clear();
}

/// One MGET position: rendered locally at dispatch, or one remote `GET`.
enum GatherPart {
    Done(Vec<u8>),
    Wait(GateWait<u64, OwnedOutcome>),
}

/// Strip the `*1\r\n` header off a single-key `JSON.MGET` sub-reply,
/// leaving the bare element (M3-S11 gather; ADR-0041 D9). Error replies
/// (fabric refusals) pass through untouched — an error is a legal RESP
/// array element.
fn strip_single_element(bytes: &[u8]) -> &[u8] {
    match bytes.strip_prefix(b"*1\r\n") {
        Some(element) => element,
        None => {
            debug_assert_eq!(bytes.first(), Some(&b'-'), "sub-replies are *1 arrays or errors");
            bytes
        }
    }
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
    /// Split MGET / JSON.MGET: per-key replies reassemble into one array
    /// in argv order. `unwrap_single` marks JSON.MGET's shape: each
    /// sub-reply is a single-key `*1` array whose element joins the outer
    /// array (ADR-0041 D9).
    Gather { parts: Vec<GatherPart>, proto: Protocol, unwrap_single: bool },
    /// Fanned MSET / scattered FLUSH: all legs must come back `+OK` (the
    /// first error leg wins the reply otherwise).
    AllOk { waiters: Vec<GateWait<u64, OwnedOutcome>>, proto: Protocol },
    /// `always` durable write (M2-S08): the reply bytes are staged but the
    /// slot resolves only once the fsync watermark covers the record's
    /// durable seq (§8.2 — ack after fsync; FIFO per connection holds).
    Durable { waiter: inf_runtime::WatermarkWait, reply: Vec<u8> },
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
    let Some((proto, id, db, conn_ns, restricted)) = shared.with_conn(key, |c| {
        (c.cx.proto, c.cx.id, c.cx.db, c.cx.ns, pubsub::subscriber_restricted(&c.cx))
    }) else {
        return FastDispatch::ConnGone;
    };
    let origin = ExecOrigin::Conn(key.slot, key.generation);
    let meta = lookup(argv[0]);
    let well_formed = meta.is_some_and(|m| arity_ok(m, argv.len()));
    if let Some(meta) = meta
        && well_formed
        && restricted
        && !pubsub::is_plane_pubsub(meta.id)
    {
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
            return match try_send_apply(shared, first_owner, proto, db, argv) {
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
async fn pump<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
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
/// multi-cell node (M1-S02). `sub` is argv[1] when present: CONFIG SET /
/// RESETSTAT and INF.NS CREATE / DROP mutate per-cell state (typed config,
/// namespace registries — M1-E3/E4) and fan out AllOk; their read forms
/// stay local.
fn is_scatter(id: CommandId, sub: Option<&[u8]>) -> bool {
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
        CommandId::InfNs => is_ns_ddl_sub(sub),
        _ => false,
    }
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
    let Some((proto, id, db, conn_ns, restricted)) = shared.with_conn(key, |c| {
        (c.cx.proto, c.cx.id, c.cx.db, c.cx.ns, pubsub::subscriber_restricted(&c.cx))
    }) else {
        return false;
    };
    let origin = ExecOrigin::Conn(key.slot, key.generation);

    let meta = lookup(argv[0]);
    let well_formed = meta.is_some_and(|m| arity_ok(m, argv.len()));
    // M1-S10: RESP2 subscriber-mode restriction for pump-dispatched
    // commands — the fast path checks inside `execute`, but commands
    // landing here would otherwise run under a synthesized ConnCx without
    // the subscription state (remote Apply, scatter legs).
    if let Some(meta) = meta
        && well_formed
        && restricted
        && !pubsub::is_plane_pubsub(meta.id)
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
        // through to the mirror arm below.
        Some(meta) if well_formed && conn_ns.is_some() && !is_conn_state(owned) => {
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
            match meta.id {
                CommandId::Dbsize => {
                    let acc = shared.apply_dbsize(origin, db);
                    let mut waiters = Vec::new();
                    for cell in peer_cells(shared) {
                        if let Ok(waiter) = send_apply(shared, cell, proto, db, &[b"DBSIZE"]).await
                        {
                            waiters.push(waiter);
                            *inflight += 1;
                        }
                    }
                    pending.push_back(PendingReply::Counted { waiters, acc, proto });
                }
                CommandId::Flushdb | CommandId::Flushall | CommandId::Config | CommandId::InfNs => {
                    // Per-cell-state mutators (flush, CONFIG SET/RESETSTAT,
                    // INF.NS CREATE/DROP): the local leg validates and
                    // applies; an error reply short-circuits the fan-out.
                    let local = run_local(shared, origin, proto, id, db, argv);
                    if local.first() == Some(&b'-') {
                        pending.push_back(PendingReply::Done(local));
                    } else {
                        shared.recycle_reply_buf(local);
                        let mut waiters = Vec::new();
                        for cell in peer_cells(shared) {
                            if let Ok(waiter) = send_apply(shared, cell, proto, db, argv).await {
                                waiters.push(waiter);
                                *inflight += 1;
                            }
                        }
                        pending.push_back(PendingReply::AllOk { waiters, proto });
                    }
                }
                CommandId::Keys => {
                    let reply = program_keys(shared, origin, proto, id, db, argv).await;
                    pending.push_back(PendingReply::Done(reply));
                }
                CommandId::Scan => {
                    let reply = program_scan(shared, origin, proto, id, db, argv).await;
                    pending.push_back(PendingReply::Done(reply));
                }
                CommandId::Randomkey => {
                    let reply = program_randomkey(shared, origin, proto, id, db, argv).await;
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
                    acc += shared.apply_counted(origin, name, k, db);
                } else {
                    match send_apply(shared, owner_of(k), proto, db, &[name, k]).await {
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
                    shared.execute_owned_into(origin, &[b"GET", k], proto, id, db, None, &mut buf);
                    parts.push(GatherPart::Done(buf));
                } else {
                    match send_apply(shared, owner_of(k), proto, db, &[b"GET", k]).await {
                        Ok(waiter) => {
                            parts.push(GatherPart::Wait(waiter));
                            *inflight += 1;
                        }
                        Err(refusal) => parts.push(GatherPart::Done(refusal)),
                    }
                }
            }
            pending.push_back(PendingReply::Gather { parts, proto, unwrap_single: false });
        }
        #[cfg(feature = "doc")]
        Some(meta) if well_formed && meta.id == CommandId::JsonMget && has_remote_key(meta) => {
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
                    shared.execute_owned_into(origin, &sub, proto, id, db, None, &mut buf);
                    parts.push(GatherPart::Done(buf));
                } else {
                    let sub: [&[u8]; 3] = [b"JSON.MGET", k, path];
                    match send_apply(shared, owner_of(k), proto, db, &sub).await {
                        Ok(waiter) => {
                            parts.push(GatherPart::Wait(waiter));
                            *inflight += 1;
                        }
                        Err(refusal) => parts.push(GatherPart::Done(refusal)),
                    }
                }
            }
            pending.push_back(PendingReply::Gather { parts, proto, unwrap_single: true });
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
                            db,
                            None,
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
                    pending.push_back(PendingReply::AllOk { waiters, proto });
                }
            }
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
            match send_apply(shared, owner, proto, db, argv).await {
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
        }) else {
            return false;
        };
        let now = shared.now.get();
        execute_slices(argv, &mut shared.store.borrow_mut(), &mut live, now, &mut reply);
        shared.observer.borrow_mut().on_execute(shared.cell, origin, argv, &reply, now);
        shared.with_conn(key, |c| {
            c.cx.proto = live.proto;
            c.cx.db = live.db;
            c.cx.ns = live.ns;
        });
    } else {
        shared.execute_owned_into(origin, argv, proto, id, db, conn_ns, &mut reply);
        if let Some(dur) = stall_request(argv) {
            shared.stall_until.set(shared.now.get().saturating_add(dur));
        }
    }
    pending.push_back(PendingReply::Done(reply));
    true
}

/// Render an owner's outcome as the RESP reply for a whole-argv `Apply`
/// (buffers come from and return to the cell's reply pool).
fn render_outcome<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
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
                OwnedOutcome::Bytes(_) | OwnedOutcome::Err(_) => unreachable!(),
            }
            reply
        }
    }
}

// ---- fabric-program helpers (M1-S02) -------------------------------------------

/// All cells except this one (scatter targets).
fn peer_cells<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Shared<O, F>,
) -> Vec<CellId> {
    (0..shared.cells).map(CellId).filter(|c| c.0 != shared.cell.0).collect()
}

fn error_reply<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Shared<O, F>,
    proto: Protocol,
    text: &str,
) -> Vec<u8> {
    let mut reply = shared.take_reply_buf();
    RespWriter::new(&mut reply, proto).error(text);
    reply
}

fn int_reply<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Shared<O, F>,
    proto: Protocol,
    n: i64,
) -> Vec<u8> {
    let mut reply = shared.take_reply_buf();
    RespWriter::new(&mut reply, proto).int(n);
    reply
}

fn simple_reply<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Shared<O, F>,
    proto: Protocol,
    text: &str,
) -> Vec<u8> {
    let mut reply = shared.take_reply_buf();
    RespWriter::new(&mut reply, proto).simple(text);
    reply
}

fn run_local<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Shared<O, F>,
    origin: ExecOrigin,
    proto: Protocol,
    id: u64,
    db: u16,
    argv: &[&[u8]],
) -> Vec<u8> {
    let mut reply = shared.take_reply_buf();
    shared.execute_owned_into(origin, argv, proto, id, db, None, &mut reply);
    reply
}

/// One program step: execute `argv` on `cell` (locally or via Apply) and
/// return its raw RESP reply bytes.
async fn run_on<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    origin: ExecOrigin,
    cell: CellId,
    proto: Protocol,
    id: u64,
    db: u16,
    argv: &[&[u8]],
) -> Vec<u8> {
    if cell.0 == shared.cell.0 {
        return run_local(shared, origin, proto, id, db, argv);
    }
    match send_apply(shared, cell, proto, db, argv).await {
        Ok(waiter) => match waiter.await {
            OwnedOutcome::Bytes(bytes) => bytes,
            outcome => render_outcome(shared, outcome, proto),
        },
        Err(refusal) => refusal,
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
) -> Result<i64, Vec<u8>> {
    if cell.0 == shared.cell.0 {
        return Ok(shared.apply_counted(origin, name, key, db));
    }
    match send_apply(shared, cell, Protocol::Resp2, db, &[name, key]).await {
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
async fn program_msetnx<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    origin: ExecOrigin,
    proto: Protocol,
    id: u64,
    db: u16,
    argv: &[&[u8]],
) -> Vec<u8> {
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
async fn program_move<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    origin: ExecOrigin,
    proto: Protocol,
    id: u64,
    db: u16,
    cmd: CommandId,
    argv: &[&[u8]],
) -> Vec<u8> {
    let (src, dst) = (argv[1], argv[2]);
    let src_owner = shared.router.cell_of(SlotRouter::slot_of(src));
    let dst_owner = shared.router.cell_of(SlotRouter::slot_of(dst));
    let mut replace = false;
    let mut dst_db = db;
    if cmd == CommandId::Copy {
        let mut i = 3;
        while i < argv.len() {
            let opt = argv[i];
            if opt.eq_ignore_ascii_case(b"REPLACE") {
                replace = true;
            } else if opt.eq_ignore_ascii_case(b"DB") && i + 1 < argv.len() {
                match crate::exec::parse_i64(argv[i + 1]) {
                    Ok(n @ 0..=15) => dst_db = n as u16,
                    Ok(_) => return error_reply(shared, proto, "ERR DB index is out of range"),
                    Err(()) => {
                        return error_reply(
                            shared,
                            proto,
                            "ERR value is not an integer or out of range",
                        );
                    }
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
        match count_on(shared, origin, dst_owner, proto, db, b"EXISTS", dst).await {
            Ok(0) => {}
            Ok(_) => return int_reply(shared, proto, 0),
            Err(error) => return error,
        }
    }
    let probe: &[u8] = if cmd == CommandId::Copy { b"INF.PEEK" } else { b"INF.TAKE" };
    let raw = run_on(shared, origin, src_owner, Protocol::Resp2, id, db, &[probe, src]).await;
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
    // COPY's destination database rides the Apply db byte (M1-S08).
    let reply = run_on(shared, origin, dst_owner, Protocol::Resp2, id, dst_db, &put).await;
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
async fn program_keys<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    origin: ExecOrigin,
    proto: Protocol,
    id: u64,
    db: u16,
    argv: &[&[u8]],
) -> Vec<u8> {
    let local = run_local(shared, origin, proto, id, db, argv);
    let Some((mut total, local_off)) = parse_array_header(&local) else {
        return local; // error passthrough
    };
    let mut waiters = Vec::new();
    for cell in peer_cells(shared) {
        match send_apply(shared, cell, proto, db, argv).await {
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
async fn program_scan<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    origin: ExecOrigin,
    proto: Protocol,
    id: u64,
    db: u16,
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
    let raw = run_on(shared, origin, CellId(target), proto, id, db, &sub).await;
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
async fn program_randomkey<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    origin: ExecOrigin,
    proto: Protocol,
    id: u64,
    db: u16,
    argv: &[&[u8]],
) -> Vec<u8> {
    let start = (crate::exec::next_rand(&shared.node) % u64::from(shared.cells)) as u16;
    for i in 0..shared.cells {
        let cell = CellId((start + i) % shared.cells);
        let raw = run_on(shared, origin, cell, proto, id, db, argv).await;
        if raw != b"$-1\r\n" && raw != b"_\r\n" {
            return raw;
        }
        shared.recycle_reply_buf(raw);
    }
    let mut reply = shared.take_reply_buf();
    RespWriter::new(&mut reply, proto).null();
    reply
}

// ---- pub/sub plane programs (M1-S10/S11) ----------------------------------------

/// Pump-side dispatch for the six public pub/sub commands. Subscribe-family
/// ops mutate the connection state, sync this cell's registries, and ship
/// the 0→1/1→0 transition deltas — **awaited before the confirmation frames
/// are emitted**, so once a client sees its confirmation, a PUBLISH from
/// anywhere reaches it. PUBLISH routes to the channel's owner; PUBSUB is an
/// introspection program over the owner views.
async fn dispatch_pubsub<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    key: ConnKey,
    id: CommandId,
    argv: &[&[u8]],
    proto: Protocol,
    pending: &mut VecDeque<PendingReply>,
    inflight: &mut usize,
) -> bool {
    match id {
        CommandId::Subscribe
        | CommandId::Psubscribe
        | CommandId::Unsubscribe
        | CommandId::Punsubscribe => {
            let kind = if matches!(id, CommandId::Subscribe | CommandId::Unsubscribe) {
                SubKind::Channel
            } else {
                SubKind::Pattern
            };
            let adding = matches!(id, CommandId::Subscribe | CommandId::Psubscribe);
            let names: Vec<&[u8]> = argv[1..].to_vec();
            let mut frames = shared.take_reply_buf();
            let Some(changes) = shared.with_conn(key, |conn| {
                if adding {
                    pubsub::apply_subscribe(&names, kind, &mut conn.cx, &mut frames)
                } else {
                    let names = (!names.is_empty()).then_some(names.as_slice());
                    pubsub::apply_unsubscribe(names, kind, &mut conn.cx, &mut frames)
                }
            }) else {
                return false;
            };
            let mut notes: Vec<(SubKind, Vec<u8>, i32)> = Vec::new();
            {
                let mut ps = shared.pubsub.borrow_mut();
                for (name, changed) in &changes {
                    if !changed {
                        continue;
                    }
                    let transition = if adding {
                        ps.local_add(kind, name, key)
                    } else {
                        ps.local_remove(kind, name, key)
                    };
                    if transition {
                        notes.push((kind, name.clone(), if adding { 1 } else { -1 }));
                    }
                }
            }
            let mut waiters = Vec::new();
            for (kind, name, delta) in &notes {
                send_sub_delta(shared, *kind, name, *delta, &mut waiters).await;
            }
            for waiter in waiters {
                let _ = waiter.await;
            }
            pending.push_back(PendingReply::Done(frames));
        }
        CommandId::Publish => {
            let (channel, payload) = (argv[1], argv[2]);
            let owner = if shared.route_local_only {
                shared.cell
            } else {
                shared.router.cell_of(SlotRouter::slot_of(channel))
            };
            if owner.0 == shared.cell.0 {
                // This cell owns the channel: deliver locally, fan one
                // INF.PUBFAN per subscriber-bearing peer, sum the typed
                // per-cell delivery counts (the Counted shape). A publisher
                // subscribed to its own channel gets its frames *after* the
                // count reply (Redis order) via a trailing Done entry.
                let (acc, self_frames) = deliver_local(shared, channel, payload, Some(key));
                let targets = shared.pubsub.borrow().fan_targets(channel, shared.cell.0);
                let mut waiters = Vec::new();
                for cell in targets {
                    let fan = &[&b"INF.PUBFAN"[..], channel, payload];
                    if let Ok(waiter) =
                        send_apply(shared, CellId(cell), Protocol::Resp2, 0, fan).await
                    {
                        note_fan(&shared.node);
                        waiters.push(waiter);
                        *inflight += 1;
                    }
                }
                pending.push_back(PendingReply::Counted { waiters, acc, proto });
                if !self_frames.is_empty() {
                    pending.push_back(PendingReply::Done(self_frames));
                }
            } else {
                let publ = &[&b"INF.PUB"[..], channel, payload];
                match send_apply(shared, owner, Protocol::Resp2, 0, publ).await {
                    Ok(waiter) => {
                        *inflight += 1;
                        pending.push_back(PendingReply::Remote { waiter, proto });
                    }
                    Err(refusal) => pending.push_back(PendingReply::Done(refusal)),
                }
            }
        }
        CommandId::Pubsub => {
            let reply = program_pubsub(shared, proto, argv).await;
            pending.push_back(PendingReply::Done(reply));
        }
        _ => unreachable!("is_plane_pubsub covers exactly the arms above"),
    }
    true
}

/// Ships one subscription transition: channel deltas go to the owner cell,
/// pattern deltas replicate to every cell (plus this cell's slot directly).
async fn send_sub_delta<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    kind: SubKind,
    name: &[u8],
    delta: i32,
    waiters: &mut Vec<GateWait<u64, OwnedOutcome>>,
) {
    let delta_arg: &[u8] = if delta < 0 { b"-1" } else { b"1" };
    let subd = &[&b"INF.SUBD"[..], kind.wire_tag(), name, delta_arg];
    match kind {
        SubKind::Channel => {
            let owner = if shared.route_local_only {
                shared.cell
            } else {
                shared.router.cell_of(SlotRouter::slot_of(name))
            };
            if owner.0 == shared.cell.0 {
                shared.pubsub.borrow_mut().apply_delta(kind, name, shared.cell.0, delta);
            } else if let Ok(waiter) = send_apply(shared, owner, Protocol::Resp2, 0, subd).await {
                waiters.push(waiter);
            }
        }
        SubKind::Pattern => {
            shared.pubsub.borrow_mut().apply_delta(kind, name, shared.cell.0, delta);
            if !shared.route_local_only {
                for cell in peer_cells(shared) {
                    if let Ok(waiter) = send_apply(shared, cell, Protocol::Resp2, 0, subd).await {
                        waiters.push(waiter);
                    }
                }
            }
        }
    }
}

/// Close-path subscription cleanup: ships the 1→0 deltas and consumes the
/// acks (nothing awaits them, but every fabric op replies — credit and
/// orphan-tripwire hygiene).
async fn flush_sub_deltas<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: Rc<Shared<O, F>>,
    notes: Vec<(SubKind, Vec<u8>, i32)>,
) {
    let mut waiters = Vec::new();
    for (kind, name, delta) in &notes {
        send_sub_delta(&shared, *kind, name, *delta, &mut waiters).await;
    }
    for waiter in waiters {
        let _ = waiter.await;
    }
}

/// Removes a closed connection from the local registries, returning the
/// cell-level transitions to notify.
fn unsubscribe_closed_conn<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    key: ConnKey,
    cx: &ConnCx,
) -> Vec<(SubKind, Vec<u8>, i32)> {
    let mut notes = Vec::new();
    let mut ps = shared.pubsub.borrow_mut();
    for channel in &cx.sub_channels {
        if ps.local_remove(SubKind::Channel, channel, key) {
            notes.push((SubKind::Channel, channel.clone(), -1));
        }
    }
    for pattern in &cx.sub_patterns {
        if ps.local_remove(SubKind::Pattern, pattern, key) {
            notes.push((SubKind::Pattern, pattern.clone(), -1));
        }
    }
    notes
}

/// The owner-side publish pump: fabric-origin PUBLISHes drain strictly in
/// arrival order — local delivery, one INF.PUBFAN per subscriber-bearing
/// peer, then the receiver-count reply to the publisher's cell. Sends leave
/// in queue order, so per-publisher delivery order holds end-to-end; reply
/// aggregation is awaited inline, making publish throughput per owner cell
/// RTT-bound — a recorded M1 simplification (no gate measures sustained
/// publish throughput; revisit with evidence per L4 if a workload demands
/// overlap).
async fn owner_pub_pump<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: Rc<Shared<O, F>>,
) {
    loop {
        let Some(item) = shared.pub_queue.borrow_mut().pop_front() else {
            shared.pub_pump_active.set(false);
            return;
        };
        let (mut total, _) = deliver_local(&shared, &item.channel, &item.payload, None);
        let targets = shared.pubsub.borrow().fan_targets(&item.channel, shared.cell.0);
        let mut waiters = Vec::new();
        for cell in targets {
            let fan = &[&b"INF.PUBFAN"[..], &item.channel, &item.payload];
            if let Ok(waiter) = send_apply(&shared, CellId(cell), Protocol::Resp2, 0, fan).await {
                note_fan(&shared.node);
                waiters.push(waiter);
            }
        }
        for waiter in waiters {
            if let OwnedOutcome::Int(n) = waiter.await {
                total += n;
            }
        }
        // Publish the reply now — the origin's pump is suspended on it.
        let mut fabric = shared.fabric.borrow_mut();
        fabric.reply(item.origin, item.token, &Outcome::Int(total));
        fabric.flush();
    }
}

/// PUBSUB introspection over the cell registries: CHANNELS merges the owner
/// views (KEYS-style header arithmetic), NUMSUB asks each channel's owner,
/// NUMPAT answers locally (the pattern index is replicated).
async fn program_pubsub<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    proto: Protocol,
    argv: &[&[u8]],
) -> Vec<u8> {
    let sub = argv[1];
    if sub.eq_ignore_ascii_case(b"CHANNELS") && argv.len() <= 3 {
        let pattern = argv.get(2).copied();
        let local = shared.pubsub.borrow().live_owned_channels(pattern);
        let mut waiters = Vec::new();
        if shared.cells > 1 && !shared.route_local_only {
            let mut request: Vec<&[u8]> = vec![b"INF.PUBSUB", b"CHANNELS"];
            if let Some(p) = pattern {
                request.push(p);
            }
            for cell in peer_cells(shared) {
                match send_apply(shared, cell, Protocol::Resp2, 0, &request).await {
                    Ok(waiter) => waiters.push(waiter),
                    Err(refusal) => return refusal,
                }
            }
        }
        let mut total = local.len();
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
        {
            let mut w = RespWriter::new(&mut reply, proto);
            w.array_header(total);
            for name in &local {
                w.bulk(name);
            }
        }
        for (bytes, off) in bodies {
            reply.extend_from_slice(&bytes[off..]);
            shared.recycle_reply_buf(bytes);
        }
        reply
    } else if sub.eq_ignore_ascii_case(b"NUMSUB") {
        enum Count {
            Local(i64),
            Wait(GateWait<u64, OwnedOutcome>),
        }
        let mut parts: Vec<(&[u8], Count)> = Vec::with_capacity(argv.len() - 2);
        for name in &argv[2..] {
            let owner = if shared.route_local_only {
                shared.cell
            } else {
                shared.router.cell_of(SlotRouter::slot_of(name))
            };
            if owner.0 == shared.cell.0 {
                parts.push((name, Count::Local(shared.pubsub.borrow().owned_count(name))));
            } else {
                let numsub = &[&b"INF.PUBSUB"[..], b"NUMSUB", name];
                match send_apply(shared, owner, Protocol::Resp2, 0, numsub).await {
                    Ok(waiter) => parts.push((name, Count::Wait(waiter))),
                    Err(refusal) => return refusal,
                }
            }
        }
        let mut reply = shared.take_reply_buf();
        RespWriter::new(&mut reply, proto).array_header(parts.len() * 2);
        for (name, count) in parts {
            let count = match count {
                Count::Local(n) => n,
                Count::Wait(waiter) => match waiter.await {
                    OwnedOutcome::Int(n) => n,
                    _ => 0,
                },
            };
            let mut w = RespWriter::new(&mut reply, proto);
            w.bulk(name);
            w.int(count);
        }
        reply
    } else if sub.eq_ignore_ascii_case(b"NUMPAT") && argv.len() == 2 {
        int_reply(shared, proto, shared.pubsub.borrow().live_pattern_count() as i64)
    } else {
        let mut reply = shared.take_reply_buf();
        pubsub::pubsub_subcommand_error(sub, &mut RespWriter::new(&mut reply, proto));
        reply
    }
}

/// Owner-side handling of the internal pub/sub Apply vocabulary. Returns
/// false when `argv` is not pub/sub plumbing (the caller falls through to
/// normal Apply execution).
fn handle_pubsub_apply<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Shared<O, F>,
    from: CellId,
    token: FabricToken,
    argv: &[&[u8]],
    scratch: &mut Vec<u8>,
    staged: &mut Vec<(CellId, FabricToken, StagedReply)>,
    pubs: &mut Vec<OwnerPub>,
) -> bool {
    let name = argv[0];
    if name.eq_ignore_ascii_case(b"INF.PUBFAN") && argv.len() == 3 {
        // Subscriber-cell delivery leg: append frames locally, reply the
        // typed per-cell receiver count. (A publisher subscribed to its own
        // channel through a remote owner may see its message frame before
        // its publish reply — recorded ordering deviation.)
        let (delivered, _) = deliver_local(shared, argv[1], argv[2], None);
        staged.push((from, token, StagedReply::Int(delivered)));
        true
    } else if name.eq_ignore_ascii_case(b"INF.PUB") && argv.len() == 3 {
        // Owner leg of a remote PUBLISH: park for the owner pump (the
        // fabric is mutably borrowed by this drain; fan-out needs sends).
        pubs.push(OwnerPub {
            origin: from,
            token,
            channel: argv[1].to_vec(),
            payload: argv[2].to_vec(),
        });
        true
    } else if name.eq_ignore_ascii_case(b"INF.SUBD") && argv.len() == 4 {
        match SubKind::from_wire_tag(argv[1]) {
            Some(kind) => {
                let delta: i32 = if argv[3] == b"-1" { -1 } else { 1 };
                shared.pubsub.borrow_mut().apply_delta(kind, argv[2], from.0, delta);
                staged.push((from, token, StagedReply::Int(0)));
            }
            None => staged.push((from, token, StagedReply::Refused)),
        }
        true
    } else if name.eq_ignore_ascii_case(b"INF.PUBSUB") && argv.len() >= 2 {
        if argv[1].eq_ignore_ascii_case(b"NUMSUB") && argv.len() == 3 {
            let count = shared.pubsub.borrow().owned_count(argv[2]);
            staged.push((from, token, StagedReply::Int(count)));
        } else if argv[1].eq_ignore_ascii_case(b"CHANNELS") && argv.len() <= 3 {
            let names = shared.pubsub.borrow().live_owned_channels(argv.get(2).copied());
            let start = scratch.len();
            let mut w = RespWriter::new(scratch, Protocol::Resp2);
            w.array_header(names.len());
            for name in &names {
                w.bulk(name);
            }
            staged.push((from, token, StagedReply::Bytes(start, scratch.len())));
        } else {
            staged.push((from, token, StagedReply::Refused));
        }
        true
    } else {
        false
    }
}

/// Delivers one published message to this cell's local subscribers:
/// complete frames append to each subscriber connection's staged output
/// (per-connection protocol — RESP3 push, RESP2 array), channel
/// subscriptions before pattern subscriptions, then the output cap
/// (M1-S11) is enforced. Returns the receiver count and — when `defer`
/// names the publishing connection — its own frames, held back so they
/// follow the publish reply (the Redis self-delivery order).
fn deliver_local<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Shared<O, F>,
    channel: &[u8],
    payload: &[u8],
    defer: Option<ConnKey>,
) -> (i64, Vec<u8>) {
    let mut delivered: i64 = 0;
    let mut deferred = Vec::new();
    for key in shared.pubsub.borrow().channel_conns(channel) {
        delivered += i64::from(deliver_one(shared, key, defer, &mut deferred, |out, proto| {
            pubsub::write_message(out, proto, channel, payload);
        }));
    }
    for (pattern, conns) in shared.pubsub.borrow().matching_pattern_conns(channel) {
        for key in conns {
            delivered += i64::from(deliver_one(shared, key, defer, &mut deferred, |out, proto| {
                pubsub::write_pmessage(out, proto, &pattern, channel, payload);
            }));
        }
    }
    let node = &shared.node;
    node.pubsub_delivered.set(node.pubsub_delivered.get() + delivered.unsigned_abs());
    (delivered, deferred)
}

fn deliver_one<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Shared<O, F>,
    key: ConnKey,
    defer: Option<ConnKey>,
    deferred: &mut Vec<u8>,
    write: impl FnOnce(&mut Vec<u8>, Protocol),
) -> bool {
    let now_ms = shared.now.get().as_millis();
    let caps = shared.cob_pubsub.get();
    shared
        .with_conn(key, |conn| {
            if conn.closing {
                return false;
            }
            if defer == Some(key) {
                // The publisher's own frames ride the reply path instead
                // (emitted right after the receiver count).
                write(deferred, conn.cx.proto);
                return true;
            }
            write(&mut conn.out, conn.cx.proto);
            enforce_output_cap(&shared.node, conn, now_ms, caps);
            true
        })
        .unwrap_or(false)
}

/// `client-output-buffer-limit pubsub` (M1-S11): the hard cap kills at
/// once; the soft cap kills after `soft_ms` continuously over. The kill is
/// the CLIENT KILL handshake (registry mark + MAINTAIN sweep close) —
/// delivery never touches another connection's I/O state directly, and the
/// connection (with its buffered output) frees on close.
fn enforce_output_cap(node: &NodeInfo, conn: &mut Conn, now_ms: u64, caps: (u64, u64, u64)) {
    let (hard, soft, soft_ms) = caps;
    if conn.cob_kill_sent {
        return;
    }
    let used = conn.out.len() as u64;
    let over_hard = hard > 0 && used > hard;
    let soft_expired = if soft > 0 && soft_ms > 0 && used > soft {
        if conn.cob_soft_since_ms == 0 {
            conn.cob_soft_since_ms = now_ms.max(1);
        }
        now_ms.saturating_sub(conn.cob_soft_since_ms) >= soft_ms
    } else {
        conn.cob_soft_since_ms = 0;
        false
    };
    if (over_hard || soft_expired) && node.clients.borrow_mut().request_kill(conn.cx.id) {
        conn.cob_kill_sent = true;
        node.cob_disconnections.set(node.cob_disconnections.get() + 1);
    }
}

fn note_fan(node: &NodeInfo) {
    node.pubsub_fan_msgs.set(node.pubsub_fan_msgs.get() + 1);
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
    extract_keys_iter(meta, argv).collect()
}

/// Non-allocating key iterator over owned slices — the dispatch hot path
/// probes key routing once per command without a `Vec` per probe (M2.5
/// Phase H allocator lever). Semantics identical to `extract_keys_slices`:
/// `first == 0` or `step == 0` yields nothing; `last < 0` counts from the
/// end; iteration stops at the argv boundary.
fn extract_keys_iter<'v, 'a>(
    meta: &inf_wire::CommandMeta,
    argv: &'v [&'a [u8]],
) -> impl Iterator<Item = &'a [u8]> + 'v {
    let spec = meta.keys;
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

/// One origin cell's namespace-apply pump (M4-S26; generalized by
/// M4.5-S27): applies arrive in fabric FIFO order and execute strictly
/// in that order — a suspended cold read (tiered) or a staging park
/// (flat under pressure, ADR-0083 D1) holds the queue behind it, which
/// is exactly what preserves the origin's per-connection command
/// ordering. Deactivates when its
/// queue drains (the flag and the emptiness check share one borrow, so
/// a concurrent enqueue always observes a live pump or respawns one).
async fn ns_apply_pump<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: Rc<Shared<O, F>>,
    origin: u16,
) {
    loop {
        let next = {
            let mut queues = shared.ns_applies.borrow_mut();
            let item = queues[usize::from(origin)].pop_front();
            if item.is_none() {
                shared.ns_pump_active.borrow_mut()[usize::from(origin)] = false;
            }
            item
        };
        let Some(item) = next else { return };
        let argv: Vec<&[u8]> = item.args.iter().map(Vec::as_slice).collect();
        // Flat durable applies ride the same FIFO under staging pressure
        // (M4.5-S27, ADR-0083 D1); the tier is re-resolved here because
        // the owner stays authoritative and DDL can retier a namespace
        // while an apply is queued.
        if !shared.store.borrow().is_tiered(item.ns) {
            apply_flat_one(&shared, origin, item.ns, &argv, item.proto, item.token).await;
            continue;
        }
        match apply_tiered_one(&shared, origin, item.ns, &argv, item.proto).await {
            tiered::TieredReply::Done(reply) => {
                shared.fabric.borrow_mut().reply(
                    CellId(origin),
                    item.token,
                    &Outcome::Bytes(&reply),
                );
                shared.recycle_reply_buf(reply);
            }
            // M4.5-S29: the durability wait leaves the pump. Holding the
            // FIFO across it serialized each origin to one `always` write
            // per fsync window — the flat-scaling defect. Staging already
            // happened (in FIFO order, no await since), so apply order is
            // intact; the reply ships from FABRIC-IN's deferred-reply
            // future once the watermark covers `seq`, and the origin
            // matches it by token — per-connection reply order is the
            // origin's pending-FIFO's job, not this queue's.
            tiered::TieredReply::Gated { reply, seq } => {
                shared
                    .durable
                    .borrow_mut()
                    .as_mut()
                    .expect("tiered implies the durable plane")
                    .note_gated_ack();
                shared.pump_gated.borrow_mut().push_back(GatedReply {
                    to: CellId(origin),
                    token: item.token,
                    seq,
                    reply,
                });
            }
        }
    }
}

/// One fabric-origin flat-namespace apply on the pump (M4.5-S27,
/// ADR-0083 D1): admission parks on `drained` — pacing, the same shape
/// the local pump has always had — and on admission success execution
/// and staging run with no await between, so apply order is the pump's
/// FIFO. A gated `always` verdict queues for FABRIC-IN's deferred-reply
/// spawn exactly like the tiered arm (ADR-0082: never awaited in FIFO
/// custody). `execute_ns_owned` counts the gated ack itself.
async fn apply_flat_one<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    origin: u16,
    ns: NsId,
    argv: &[&[u8]],
    proto: Protocol,
    token: FabricToken,
) {
    loop {
        let mut buf = shared.take_reply_buf();
        match shared.execute_ns_owned(CellId(origin), argv, proto, ns, &mut buf) {
            NsApplyOutcome::Park => {
                shared.recycle_reply_buf(buf);
                let wait = {
                    let mut durable = shared.durable.borrow_mut();
                    let cell = durable.as_mut().expect("durable admission parked");
                    cell.note_parked();
                    cell.drained.wait(())
                };
                wait.await;
            }
            NsApplyOutcome::Reply => {
                shared.fabric.borrow_mut().reply(CellId(origin), token, &Outcome::Bytes(&buf));
                shared.recycle_reply_buf(buf);
                return;
            }
            NsApplyOutcome::Gated(seq) => {
                shared.pump_gated.borrow_mut().push_back(GatedReply {
                    to: CellId(origin),
                    token,
                    seq,
                    reply: buf,
                });
                return;
            }
        }
    }
}

/// One fabric-origin tiered apply: validate and execute through the
/// tiered arm. An `always` write returns its gated verdict — the caller
/// queues it for the deferred-reply future (§8.2: the client-visible ack
/// never precedes the owner's fsync), never awaiting it in FIFO custody.
async fn apply_tiered_one<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    origin: u16,
    ns: NsId,
    argv: &[&[u8]],
    proto: Protocol,
) -> tiered::TieredReply {
    let Some(meta) = lookup(argv[0]) else {
        return tiered::TieredReply::Done(error_reply(shared, proto, "ERR unknown command"));
    };
    if !arity_ok(meta, argv.len()) {
        return tiered::TieredReply::Done(error_reply(
            shared,
            proto,
            "ERR wrong number of arguments",
        ));
    }
    let class = shared.store.borrow().ns_fsync_class(ns);
    let origin_cell = ExecOrigin::Fabric(CellId(origin));
    tiered::dispatch_tiered(shared, origin_cell, ns, meta, argv, proto, class).await
}

/// One compaction read chain (M4-S26 driving ADR-0059 D2): chunked cold
/// reads of the candidate through `ColdReads` (`ReadClass::Maintain`),
/// each chunk fed to `TieredTable::compaction_apply` at the exact scan
/// cursor. The chain ends on slice exhaustion, a tail stall, scan
/// completion, a pinned walk (relocating mid-walk is the D9-1 duplicate
/// hazard), or a read refusal/error — the cursor persists and the next
/// MAINTAIN resumes it. No borrow is held across an await (§3.3).
async fn compact_pump<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: Rc<Shared<O, F>>,
    read: crate::tier_cell::CompactRead,
) {
    let mut cursor = read.addr.to_raw();
    let mut budget = read.len;
    // Oversized-record assembly target (0 = one pool window).
    let mut need: usize = 0;
    'chain: while budget > 0 {
        let mut chunk: Vec<u8> = Vec::new();
        loop {
            let (wait, frames, skip) = {
                let tier = shared.tier.borrow();
                let Some(t) = tier.as_ref().and_then(|t| t.ns(read.ns)) else { break 'chain };
                let Some(cold) = tier.as_ref().and_then(|t| t.cold.clone()) else { break 'chain };
                let at = cursor + chunk.len() as u64;
                let want = if need > 0 { need - chunk.len() } else { 1 };
                let Some(addr) = inf_store::LogicalAddr::from_raw(at) else { break 'chain };
                // A retired-file miss re-resolves next round, never errors.
                let Some((fd, file, offset, frames, skip)) = t.plan_cold_read(addr, want) else {
                    break 'chain;
                };
                let len = frames as usize * inf_log::TIER_FRAME_BYTES;
                // Same-clock stamp as `on_completion` (the
                // `cold_read_p99_us` pair).
                let now_us = shared.now.get().as_micros();
                match cold.enqueue(fd, file, offset, len, inf_runtime::ReadClass::Maintain, now_us)
                {
                    Ok(wait) => (wait, frames, skip),
                    Err(_) => break 'chain, // queue full: back off to the next round
                }
            };
            let done = wait.await;
            if done.outcome().is_err() {
                break 'chain; // typed read failure: cursor persists, retried
            }
            let extracted = done.bytes(|window| {
                let window_data = frames as usize * inf_log::TIER_FRAME_DATA - skip;
                let take = if need > 0 { window_data.min(need - chunk.len()) } else { window_data };
                let mut piece = Vec::new();
                inf_log::tier_extract(window, skip, take, &mut piece).ok().map(|()| piece)
            });
            drop(done);
            match extracted {
                Some(piece) => chunk.extend_from_slice(&piece),
                None => break 'chain, // frame CRC failure: foreground reads surface it typed
            }
            if need == 0 || chunk.len() >= need {
                break;
            }
        }
        let applied = {
            let mut ks = shared.store.borrow_mut();
            let Some(table) = ks.tiered_store_mut(read.ns) else { break 'chain };
            // A walk pinned itself between chunks: relocating now would
            // let one walk emit a ref and an image for the same key
            // (ADR-0059 D9-1) — pause; the cursor resumes post-walk.
            if table.space().walk_watermark().is_some() {
                break 'chain;
            }
            let Some(addr) = inf_store::LogicalAddr::from_raw(cursor) else { break 'chain };
            table.compaction_apply(read.file_id, addr, &chunk)
        };
        if applied.consumed > 0 {
            cursor += applied.consumed;
            budget = budget.saturating_sub(applied.consumed);
            need = 0;
        }
        if applied.file_scanned || applied.stalled {
            break 'chain;
        }
        if applied.need > 0 {
            need = usize::try_from(applied.need).expect("record sizes fit usize");
            continue;
        }
        if applied.consumed == 0 {
            debug_assert!(false, "compaction_apply made no progress without a verdict");
            break 'chain;
        }
    }
    if let Some(tier) = shared.tier.borrow_mut().as_mut()
        && let Some(t) = tier.ns_mut(read.ns)
    {
        t.compact_inflight = false;
    }
}

/// One named-namespace command on the pump (M2-S08). Returns `false` when
/// the connection is gone.
#[allow(clippy::too_many_arguments)] // the pump dispatch context
async fn dispatch_ns<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    key: ConnKey,
    origin: ExecOrigin,
    meta: &'static inf_wire::CommandMeta,
    argv: &[&[u8]],
    proto: Protocol,
    id: u64,
    db: u16,
    ns: NsId,
    pending: &mut VecDeque<PendingReply>,
    inflight: &mut usize,
) -> bool {
    // The registry is authoritative: a dropped namespace answers a typed
    // error before any routing.
    let class = {
        let ks = shared.store.borrow();
        if ks.ns_get_by_id(ns).is_none() {
            pending.push_back(PendingReply::Done(error_reply(
                shared,
                proto,
                "ERR the selected namespace was dropped (INF.NS USE again)",
            )));
            return true;
        }
        ks.ns_fsync_class(ns)
    };
    let keys = extract_keys_slices(meta, argv);
    let owner_of = |k: &[u8]| shared.router.cell_of(SlotRouter::slot_of(k));
    if !keys.is_empty() && !shared.route_local_only {
        let owner = owner_of(keys[0]);
        if keys[1..].iter().any(|k| owner_of(k) != owner) {
            // M3-S11: the JSON surface binds the named-ns multi-key
            // programs (exactly the ADR-0032 D5 plan) — the MGET gather
            // with ns-aware sub-ops. The generic refusal below stays for
            // every other command until M5.
            #[cfg(feature = "doc")]
            if meta.id == CommandId::JsonMget {
                let path = argv[argv.len() - 1];
                let mut parts = Vec::with_capacity(argv.len() - 2);
                for k in &argv[1..argv.len() - 1] {
                    let sub: [&[u8]; 3] = [b"JSON.MGET", k, path];
                    if shared.router.is_local(k, shared.cell) {
                        let mut buf = shared.take_reply_buf();
                        shared.execute_owned_into(origin, &sub, proto, id, db, Some(ns), &mut buf);
                        parts.push(GatherPart::Done(buf));
                    } else {
                        match send_apply_ns(shared, owner_of(k), proto, ns, &sub).await {
                            Ok(waiter) => {
                                parts.push(GatherPart::Wait(waiter));
                                *inflight += 1;
                            }
                            Err(refusal) => parts.push(GatherPart::Done(refusal)),
                        }
                    }
                }
                pending.push_back(PendingReply::Gather { parts, proto, unwrap_single: true });
                return true;
            }
            // Recorded M2 limitation (ADR-0015 deviations): multi-key
            // commands spanning cells bind with the M3 named-ns programs.
            pending.push_back(PendingReply::Done(error_reply(
                shared,
                proto,
                "ERR multi-key commands spanning cells are not yet supported in named namespaces (M2)",
            )));
            return true;
        }
        if owner.0 != shared.cell.0 {
            match send_apply_ns(shared, owner, proto, ns, argv).await {
                Ok(waiter) => {
                    *inflight += 1;
                    pending.push_back(PendingReply::Remote { waiter, proto });
                }
                Err(refusal) => pending.push_back(PendingReply::Done(refusal)),
            }
            return true;
        }
    }
    // Tiered namespaces execute through the async tiered arm (M4-S26):
    // suspension-capable resolution with its own admission + staging.
    // Keyspace-level commands (INFO, CONFIG, INF.NS, SELECT, pub/sub)
    // stay on the ordinary path — `execute` owns them regardless of the
    // selected namespace.
    let keyspace_level = matches!(
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
    );
    if !keyspace_level && shared.store.borrow().is_tiered(ns) {
        match tiered::dispatch_tiered(shared, origin, ns, meta, argv, proto, class).await {
            tiered::TieredReply::Done(reply) => pending.push_back(PendingReply::Done(reply)),
            tiered::TieredReply::Gated { reply, seq } => {
                let waiter = {
                    let mut durable = shared.durable.borrow_mut();
                    let cell = durable.as_mut().expect("tiered implies the durable plane");
                    cell.note_gated_ack();
                    cell.ack_gate.waiter(seq)
                };
                pending.push_back(PendingReply::Durable { waiter, reply });
            }
        }
        return true;
    }
    // Local path: durable admission parks on the drain waitlist instead of
    // erroring — with ADR-0083 the fabric path paces the same way, so the
    // typed verdict is shared and the two paths cannot drift (M4.5-S27).
    let is_write = meta.flags.contains(CmdFlags::WRITE);
    if class.is_some() && is_write {
        loop {
            match shared.durable_admission(ns, meta, argv) {
                DurableAdmission::Admit => break,
                DurableAdmission::Refuse(refusal) => {
                    pending.push_back(PendingReply::Done(error_reply(shared, proto, refusal)));
                    return true;
                }
                DurableAdmission::Park => {
                    let wait = {
                        let mut durable = shared.durable.borrow_mut();
                        let cell = durable.as_mut().expect("durable admission ran");
                        cell.note_parked();
                        cell.drained.wait(())
                    };
                    wait.await;
                }
            }
        }
    }
    // Maintenance bracket, local pump named-ns row (ADR-0072 D3 /
    // ADR-0076 D3 row 2): pre-half after the admission loop, before
    // execute; commit-half after the staging window.
    #[cfg(feature = "doc")]
    let bracket: Option<Option<inf_doc::PathProgram>> = if is_write
        && meta.id != CommandId::Copy
        && !keys.is_empty()
        && shared.store.borrow().ns_indexed(ns)
    {
        Some(shared.json_mutation_path(meta.id, argv))
    } else {
        None
    };
    #[cfg(feature = "doc")]
    if let Some(path) = &bracket
        && let Err(refusal) = shared.store.borrow_mut().idx_bracket_begin(ns, &keys, path.as_ref())
    {
        pending.push_back(PendingReply::Done(error_reply(shared, proto, refusal.message())));
        return true;
    }
    let mut reply = shared.take_reply_buf();
    shared.execute_owned_into(origin, argv, proto, id, db, Some(ns), &mut reply);
    let gated = if is_write
        && reply.first() != Some(&b'-')
        && let Some(class) = class
        && let Some(seq) = shared.stage_durable_effects(ns, meta, argv, class)
        && class == FsyncClass::Always
    {
        Some(seq)
    } else {
        None
    };
    #[cfg(feature = "doc")]
    if bracket.is_some() {
        shared.store.borrow_mut().idx_bracket_commit(ns, &keys);
    }
    if let Some(seq) = gated {
        let waiter = {
            let mut durable = shared.durable.borrow_mut();
            let cell = durable.as_mut().expect("staged above");
            cell.note_gated_ack();
            cell.ack_gate.waiter(seq)
        };
        pending.push_back(PendingReply::Durable { waiter, reply });
        return shared.with_conn(key, |_| ()).is_some();
    }
    pending.push_back(PendingReply::Done(reply));
    true
}

/// `INF.CKPT [CELL k] [WAIT]` + `BGSAVE`/`LASTSAVE` (M2-S20, ADR-0021
/// D6): requests bump the control board's epochs (cells run checkpoints
/// in their own MAINTAIN slices — L1); `WAIT` parks the pump until every
/// targeted slot's **published** epoch covers the request. Publication
/// happens at the MANIFEST swap's dir-fsync commit, so `WAIT` returns
/// only after durability — a swap abort does not publish; the retried
/// swap does (fault-injection verified). `LASTSAVE` = unix seconds of
/// the newest publication across cells (0 before the first — deviation
/// documented; Redis reports process-start time).
async fn program_ckpt<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    proto: Protocol,
    id: CommandId,
    argv: &[&[u8]],
) -> Vec<u8> {
    let no_plane = "ERR checkpointing requires a durable node (no data dir)";
    let control = shared.control.borrow().clone();
    let Some(control) = control else {
        return error_reply(shared, proto, no_plane);
    };
    if shared.durable.borrow().is_none() {
        return error_reply(shared, proto, no_plane);
    }
    if id == CommandId::Lastsave {
        let unix_s = control.ckpt_board().max_unix_ms() / 1000;
        return int_reply(shared, proto, unix_s as i64);
    }
    let mut cell: Option<u16> = None;
    let mut wait = false;
    if id == CommandId::InfCkpt {
        let mut i = 1;
        while i < argv.len() {
            if argv[i].eq_ignore_ascii_case(b"WAIT") {
                wait = true;
                i += 1;
            } else if argv[i].eq_ignore_ascii_case(b"CELL") && i + 1 < argv.len() {
                let parsed = core::str::from_utf8(argv[i + 1]).ok().and_then(|s| s.parse().ok());
                let Some(k) = parsed.filter(|&k: &u16| k < shared.cells) else {
                    return error_reply(
                        shared,
                        proto,
                        "ERR CELL wants an index below the cell count",
                    );
                };
                cell = Some(k);
                i += 2;
            } else {
                return error_reply(shared, proto, "ERR syntax: INF.CKPT [CELL k] [WAIT]");
            }
        }
    } else if argv.len() > 2 || (argv.len() == 2 && !argv[1].eq_ignore_ascii_case(b"SCHEDULE")) {
        // BGSAVE [SCHEDULE]: SCHEDULE is accepted and moot — checkpoints
        // never fork, so there is nothing to defer (deviation documented).
        return error_reply(shared, proto, "ERR syntax: BGSAVE [SCHEDULE]");
    }
    let epoch = match cell {
        Some(k) => control.request_ckpt_cell(k),
        None => control.request_ckpt_all(),
    };
    if wait {
        loop {
            let board = control.ckpt_board();
            let satisfied = match cell {
                Some(k) => board.slot(k).published() >= epoch,
                None => board.min_published() >= epoch,
            };
            if satisfied {
                break;
            }
            shared.ckpt_waiters.wait(0).await;
        }
    }
    if id == CommandId::Bgsave {
        simple_reply(shared, proto, "Background saving started")
    } else {
        simple_reply(shared, proto, "OK")
    }
}

/// The namespace-DDL program (M2-S08, ADR-0015 D2/D3): parse → allocate id
/// (CREATE) → apply locally → fan `INF.NSFAN` to every peer (AllOk) →
/// persist the catalog through the control thread → `+OK` only after the
/// swap is durable.
async fn program_ns_ddl<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    origin: ExecOrigin,
    proto: Protocol,
    id: u64,
    db: u16,
    argv: &[&[u8]],
) -> Vec<u8> {
    let _ = (origin, id, db);
    let Some(control) = shared.control.borrow().clone() else {
        return error_reply(
            shared,
            proto,
            "ERR namespace DDL requires the node control plane (planeless tier is read-only here)",
        );
    };
    let create = argv[1].eq_ignore_ascii_case(b"CREATE");
    let fan: Vec<Vec<u8>> = if create {
        let draft = match crate::admin::parse_ns_create(argv) {
            Ok(draft) => draft,
            Err(msg) => return error_reply(shared, proto, &msg),
        };
        if draft.mode == NsMode::Durable && shared.durable.borrow().is_none() {
            return error_reply(
                shared,
                proto,
                "ERR this node has no durable storage (start infinityd with a data dir)",
            );
        }
        let ns_id = control.alloc_ns_id();
        let spec = draft.with_id(ns_id);
        if let Err(e) = shared.store.borrow_mut().ns_create(spec.clone()) {
            let mut reply = shared.take_reply_buf();
            crate::admin::ns_error(e, &mut RespWriter::new(&mut reply, proto));
            return reply;
        }
        let fsync = spec.fsync.map_or("-", |f| match f {
            FsyncClass::Everysec => "everysec",
            FsyncClass::Always => "always",
        });
        let policy = spec.policy.map_or("-", inf_store::EvictionPolicy::name);
        let maxmemory = spec.maxmemory.map_or_else(|| "-".to_string(), |b| b.to_string());
        vec![
            b"INF.NSFAN".to_vec(),
            b"CREATE".to_vec(),
            spec.name.clone(),
            spec.mode.name().as_bytes().to_vec(),
            fsync.as_bytes().to_vec(),
            policy.as_bytes().to_vec(),
            maxmemory.into_bytes(),
            ns_id.to_string().into_bytes(),
            tier_to_fan(spec.tier.as_ref()),
        ]
    } else if argv[1].eq_ignore_ascii_case(b"SET") {
        // M4-S19 (ADR-0062 D3) / M4-S27 (ADR-0068 D3): hot-reload is DDL
        // — registry + store update locally, fan to peers, catalog
        // persist-then-ack.
        let (name, update) = {
            let store = shared.store.borrow();
            match crate::admin::parse_ns_set(argv, &store) {
                Ok(parsed) => parsed,
                Err(msg) => return error_reply(shared, proto, &msg),
            }
        };
        let fan_tail = match &update {
            crate::admin::NsSetUpdate::Tier(tier) => vec![tier_to_fan(Some(tier))],
            crate::admin::NsSetUpdate::MemoryPressure { policy, maxmemory } => vec![
                b"MEMCFG".to_vec(),
                policy.map_or_else(|| b"-".to_vec(), |p| p.name().as_bytes().to_vec()),
                maxmemory.map_or_else(|| b"-".to_vec(), |b| b.to_string().into_bytes()),
            ],
        };
        if let Err(e) = crate::admin::apply_ns_set(&mut shared.store.borrow_mut(), &name, update) {
            let mut reply = shared.take_reply_buf();
            crate::admin::ns_error(e, &mut RespWriter::new(&mut reply, proto));
            return reply;
        }
        let mut fan = vec![b"INF.NSFAN".to_vec(), b"SET".to_vec(), name];
        fan.extend(fan_tail);
        fan
    } else {
        if argv.len() != 3 {
            return error_reply(shared, proto, "ERR wrong number of arguments for 'INF.NS|DROP'");
        }
        if let Err(e) = shared.store.borrow_mut().ns_drop(argv[2]) {
            let mut reply = shared.take_reply_buf();
            crate::admin::ns_error(e, &mut RespWriter::new(&mut reply, proto));
            return reply;
        }
        vec![b"INF.NSFAN".to_vec(), b"DROP".to_vec(), argv[2].to_vec()]
    };
    // Fan to peers (AllOk — partial failure surfaces as the first error
    // leg, the recorded M1 scatter semantics).
    let fan_argv: Vec<&[u8]> = fan.iter().map(Vec::as_slice).collect();
    let mut failure: Option<Vec<u8>> = None;
    for cell in peer_cells(shared) {
        if let Ok(waiter) = send_apply(shared, cell, proto, 0, &fan_argv).await
            && let OwnedOutcome::Bytes(bytes) = waiter.await
            && bytes.first() == Some(&b'-')
            && failure.is_none()
        {
            failure = Some(bytes);
        }
    }
    if let Some(error) = failure {
        return error;
    }
    // Persist the catalog; ack only once the swap is durable (a DDL whose
    // definition can vanish after +OK would be a §8.2 violation).
    let epoch = control.request_persist(shared.store.borrow().export_catalog(
        control.next_ns_id(),
        control.next_index_id(),
        control.next_index_generation(),
    ));
    while !control.persisted(epoch) {
        shared.ddl_waiters.wait(0).await;
    }
    simple_reply(shared, proto, "OK")
}

/// Ship `argv` to `to` as an `ApplyNs` (named-namespace op — ADR-0015 D1)
/// and return the reply waiter. Mirrors [`send_apply`]; never RTT-recorded
/// (`always` replies are owner-deferred and would mispair the queue).
async fn send_apply_ns<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    to: CellId,
    proto: Protocol,
    ns: NsId,
    argv: &[&[u8]],
) -> Result<GateWait<u64, OwnedOutcome>, Vec<u8>> {
    let Some(args) = ApplyArgs::new(argv) else {
        let mut reply = Vec::new();
        RespWriter::new(&mut reply, proto).error("ERR too many arguments for cross-cell execution");
        return Err(reply);
    };
    let slot = SlotRouter::slot_of(argv.get(1).copied().unwrap_or(b""));
    let (token, waiter) = {
        let mut fabric = shared.fabric.borrow_mut();
        let token = fabric.next_token();
        (token, shared.gate.waiter(token.0))
    };
    let proto_byte: u8 = match proto {
        Protocol::Resp3 => 3,
        Protocol::Resp2 => 2,
    };
    loop {
        let op = Op::ApplyNs { token, slot, cmd: proto_byte, ns: ns.0, args };
        let sent = shared.fabric.borrow_mut().send(to, &op);
        match sent {
            Ok(()) => break,
            Err(SendError::NoCredit { .. }) => shared.credit_waiters.wait(to).await,
        }
    }
    Ok(waiter)
}

/// Outcome of a synchronous [`try_send_apply`] first attempt (de-async
/// fast path, ADR-0030 D4).
enum SendNow {
    /// Staged on the first attempt; the reply waiter is registered.
    Sent(GateWait<u64, OwnedOutcome>),
    /// The argv exceeds the codec's argument cap; carries the refusal.
    Refused(Vec<u8>),
    /// No fabric credit on the first attempt — the caller falls back to
    /// the async path, which waits for credits. The drawn token is
    /// abandoned: a skipped monotonic value, never registered and never
    /// sent, so no reply or RTT pairing can ever reference it.
    NoCredit,
}

/// Synchronous first-attempt [`send_apply`]: token draw + stage in one
/// fabric borrow, waiter registered only after a successful stage (safe
/// for the same reason as `send_apply`'s post-staging registration — the
/// peer cannot observe the op until FABRIC-OUT publishes it, after this
/// synchronous stretch).
fn try_send_apply<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    to: CellId,
    proto: Protocol,
    db: u16,
    argv: &[&[u8]],
) -> SendNow {
    let Some(args) = ApplyArgs::new(argv) else {
        let mut reply = Vec::new();
        RespWriter::new(&mut reply, proto).error("ERR too many arguments for cross-cell execution");
        return SendNow::Refused(reply);
    };
    let slot = SlotRouter::slot_of(argv.get(1).copied().unwrap_or(b""));
    let proto_byte: u8 = match proto {
        Protocol::Resp3 => 3,
        Protocol::Resp2 => 2,
    };
    debug_assert!(db < 16, "db rides 4 bits of the Apply cmd byte");
    let cmd_byte = proto_byte | ((db as u8) << 4);
    let (token, sent) = {
        let mut fabric = shared.fabric.borrow_mut();
        let token = fabric.next_token();
        let sent = fabric.send(to, &Op::Apply { token, slot, cmd: cmd_byte, args });
        (token, sent)
    };
    if sent.is_err() {
        return SendNow::NoCredit;
    }
    let waiter = shared.gate.waiter(token.0);
    if argv.first().copied() != Some(&b"INF.PUB"[..]) {
        shared.rtt_sent.borrow_mut()[usize::from(to.0)].push_back((token.0, shared.now.get()));
    }
    SendNow::Sent(waiter)
}

/// Ship `argv` to `to` as an `Apply` and return the reply waiter, waiting
/// for fabric credits when exhausted (backpressure, never unbounded
/// queueing). The send time is queued for delivery-side RTT recording.
/// `Err` carries the refusal reply when the argv exceeds the codec's
/// argument cap.
async fn send_apply<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    to: CellId,
    proto: Protocol,
    db: u16,
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
    // `cmd` packs `{db:4 | proto:4}` (ADR-0009) — SELECT travels with the
    // op on the byte the codec already had; db is < 16 by SELECT bounds.
    let proto_byte: u8 = match proto {
        Protocol::Resp3 => 3,
        Protocol::Resp2 => 2,
    };
    debug_assert!(db < 16, "db rides 4 bits of the Apply cmd byte");
    let cmd_byte = proto_byte | ((db as u8) << 4);
    // Token draw + first send attempt share one fabric borrow (M2.5
    // Phase H). Registering the waiter *after* staging stays safe: `send`
    // only stages into the outbound pack — the peer cannot observe the op
    // until FABRIC-OUT publishes it, after this synchronous stretch — so
    // no reply can precede the registration (and the gate parks any value
    // arriving before the waiter's first poll regardless).
    let (token, mut sent) = {
        let mut fabric = shared.fabric.borrow_mut();
        let token = fabric.next_token();
        let sent = fabric.send(to, &Op::Apply { token, slot, cmd: cmd_byte, args });
        (token, sent)
    };
    let waiter = shared.gate.waiter(token.0);
    while let Err(SendError::NoCredit { .. }) = sent {
        shared.credit_waiters.wait(to).await;
        let op = Op::Apply { token, slot, cmd: cmd_byte, args };
        sent = shared.fabric.borrow_mut().send(to, &op);
    }
    // RTT pairing relies on in-order replies; `INF.PUB` replies are deferred
    // by the owner pump (fan acks first), so its hops are not RTT samples —
    // the fan legs (`INF.PUBFAN`) cover pub/sub in the histogram instead.
    if argv.first().copied() != Some(&b"INF.PUB"[..]) {
        shared.rtt_sent.borrow_mut()[usize::from(to.0)].push_back((token.0, shared.now.get()));
    }
    Ok(waiter)
}
