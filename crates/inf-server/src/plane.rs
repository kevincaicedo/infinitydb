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
    ApplyArgs, CellFabric, ErrCode, FabricToken, MAX_APPLY_ARGS, MAX_INLINE_APPLY_ARGS, Op,
    Outcome, SendError,
};
use inf_foundation::time::Nanos;
use inf_foundation::{CellId, LogHistogram};
#[cfg(feature = "doc")]
use inf_log::DocLineage;
use inf_log::fs::{SegmentFs, StdSegmentFs};
use inf_log::{FsyncClass, MutationEffect, SegmentRotor};
use inf_runtime::GroupClass;
use inf_runtime::{
    Admission, CellPlane, Completion, CompletionResult, CompletionToken, FabricGate, GateWait,
    IoClass, IoOp, LoopCx, RawFd, TokenClass, WaitList,
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

mod cell_loop;
mod conn;
mod dispatch;
mod fabric;
mod ns_ddl;
mod publish;
mod pumps;
mod scatter;
mod shared;
mod tiered;

pub use conn::OwnedOutcome;
use conn::{ACCEPT_RETRY, ACCEPT_RETRY_TIMER_KEY, CONN_SLOT_CAP, MAXCLIENTS_REFUSAL, OwnedCmd};
use dispatch::{PendingReply, close_after_reply, is_scatter, pump, render_outcome};
use fabric::{
    GatherPart, StagedApply, flush_apply_stage, flush_parse_stage, handle_fabric_op,
    parse_stageable, stage_argv_block_argv, stage_or_handle, strip_single_element,
};
use ns_ddl::{
    SendNow, dispatch_ns, program_ckpt, program_ns_ddl, send_apply, send_apply_ns, try_send_apply,
};
use publish::{
    dispatch_pubsub, enforce_output_cap, flush_sub_deltas, handle_pubsub_apply, owner_pub_pump,
    unsubscribe_closed_conn,
};
use pumps::{compact_pump, ns_apply_pump, shadow_pump};
use scatter::{
    CountedFold, ScatterScope, error_reply, extract_keys_iter, extract_keys_slices, int_reply,
    keyspace_level, ns_dbsize_local, parse_publisher_tag, peer_cells, program_keys, program_move,
    program_msetnx, program_randomkey, program_scan, return_error, run_local, simple_reply,
    stages_despite_error, u64_decimal,
};
pub use scatter::{fold_live_entries, parse_array_header, parse_scan_head, parse_take_reply};
use shared::{DurableAdmission, NsApplyOutcome, handle_ns_apply, tier_to_fan};

use crate::control::{ControlHandle, RecoveryBoard};
use crate::durable::{DurableCell, DurableConfig, EVERYSEC_TIMER_KEY};
#[cfg(feature = "doc")]
use crate::exec::DocLogAdmission;
use crate::exec::{
    ConnCx, ConnNamespace, NodeInfo, execute, execute_slices, stall_request,
    unavailable_default_allows,
};
use crate::pubsub::{self, PubSubCell, SubKind};
use crate::recover::{RecoverPhase, Recovery, RecoveryProgress};

// ---- shared data definitions (behaviour lives in the child modules) ----------

/// One local fast-path command staged by the parse-batch prefetch (M2.5
/// Phase H, ADR-0029 lever 2 — the ADR-0005 pipeline shape on the batch the
/// parse loop naturally provides): argv flat-copied into the stage scratch,
/// key hash computed (and index probe lines prefetched) at stage time.
/// Execution reads `ConnCx` live at flush, so stage-time state is only ever
/// a prefetch hint — never an execution input (conn-state mutators are
/// flush barriers besides).
pub(super) struct StagedParse {
    /// Offset of the flat argv block in the stage scratch.
    off: u32,
    /// `hasher.hash(argv[1])` when the command carries a key.
    hash: u64,
    has_key: bool,
}

/// Generation-keyed connection slab. Generic over the entry so the
/// admission bound is unit-testable without building a `Conn`.
pub(super) struct ConnSlab<T = Conn> {
    slots: Vec<Option<T>>,
    gens: Vec<u32>,
    free: Vec<u32>,
    live: usize,
    /// Slots this slab may allocate (`CONN_SLOT_CAP` in production; a
    /// small number in the admission test).
    cap: u32,
}

/// One fabric-origin namespace apply parked for its origin's FIFO pump
/// (M4-S26 tiered; M4.5-S27 added flat durable applies under staging
/// pressure — the suspension-capable sibling of the gated-reply future).
pub(super) struct NsApply {
    token: FabricToken,
    ns: NsId,
    proto: Protocol,
    args: Vec<Vec<u8>>,
    /// The frame's program mark (ADR-0115), carried to the pump.
    program: bool,
}

/// One fabric-origin PUBLISH parked at the owner cell.
pub(super) struct OwnerPub {
    origin: CellId,
    token: FabricToken,
    channel: Vec<u8>,
    payload: Vec<u8>,
    /// The publisher tag `(conn, seq)` as the origin sent it (ADR-0101
    /// D1/D2): opaque here, echoed on the origin cell's fan leg only.
    tag: Option<(Vec<u8>, Vec<u8>)>,
}

pub(super) struct Conn {
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
    /// Injected-clock ms of the last received buffer (accept counts);
    /// the `timeout` reaper's input (ADR-0123 D2) — one store per
    /// `Recv`, never per command.
    last_active_ms: u64,
    /// Remote-publish sequence (ADR-0101 D1): a subscribed connection's
    /// forwarded `PUBLISH` carries `(key, seq)` so the owner's fan leg
    /// back to this cell can pair the publisher's own frames with its
    /// own reply. Never advances for unsubscribed publishers.
    publish_seq: u64,
    /// The publisher's own frames a tagged fan leg delivered, keyed by
    /// sequence; the pump emits each right after that publish's count
    /// reply (ADR-0101 D3/D4). Bounded by the in-flight window and
    /// empty for every connection that is not a self-subscribed
    /// publisher.
    self_push: Vec<(u64, Vec<u8>)>,
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub(super) struct ConnKey {
    slot: u32,
    generation: u32,
}

/// One owner-side `always` reply deferred on the fsync watermark.
pub(super) struct GatedReply {
    to: CellId,
    token: FabricToken,
    seq: u64,
    reply: Vec<u8>,
}

pub(super) struct Shared<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static> {
    cell: CellId,
    cells: u16,
    router: SlotRouter,
    /// The node's key hasher (ADR-0094) — the keyspace's own value,
    /// copied so the stage-time prefetch hashes exactly as the store
    /// probes (no ambient constant, no second source of truth).
    hasher: inf_foundation::KeyHasher,
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
    /// Accept completions that failed (F-L11-05): `INFO stats
    /// accept_errors`, never connection housekeeping.
    accept_errors: Cell<u64>,
    /// Leaf ops dropped from a batch nested inside a batch (F-L18-06).
    /// The codec refuses that shape on the wire, so this counts only an
    /// in-process producer that bypassed it — a tripwire, not a stat.
    nested_batch_ops_dropped: Cell<u64>,
    /// Pub/sub registries (M1-S10): local subscriber lists, owner-side
    /// per-cell counts, the replicated pattern index.
    pubsub: RefCell<PubSubCell<ConnKey>>,
    /// Fabric-origin PUBLISHes awaiting this cell's owner pump (FIFO — the
    /// queue preserves per-publisher delivery order across fan-outs).
    pub_queue: RefCell<VecDeque<OwnerPub>>,
    pub_pump_active: Cell<bool>,
    /// The connection knobs (ADR-0123 D5): `maxclients` share, `timeout`,
    /// `tcp-keepalive`, both output-buffer classes — read at assembly,
    /// refreshed by the MAINTAIN config sweep.
    knobs: Cell<crate::config::ConnKnobs>,
    /// `proto-max-bulk-len` as parser limits (ADR-0122): taken by every
    /// accept, pushed to every live parser by the same sweep.
    parser_limits: Cell<ParserLimits>,
    /// The durable plane (M2-S08, ADR-0015): `None` = memory-only cell —
    /// the zero-cost branch every memory-path check reduces to (M2-S09).
    durable: RefCell<Option<DurableCell<F>>>,
    /// The tiered plane half (M4-S26): flush pipelines, cold-read
    /// custody, MAINTAIN drivers. `None` until the durable plane exists
    /// (a tiered namespace is a configuration of `MODE durable` —
    /// ADR-0062 D1); inner state stays empty until one materializes.
    tier: RefCell<Option<crate::tier_cell::TierCell<F>>>,
    /// Dropped namespaces awaiting their catalog persist (ADR-0100 D5):
    /// `(ns, epoch)` recorded by the DROP program (origin) or the
    /// `INF.NSFAN DROP` apply (peers) before MAINTAIN parks the tier
    /// files; consumed by `TierCell::sync_namespaces`. Bounded by DDL
    /// rate — one entry per drop, consumed at the next MAINTAIN.
    ns_drop_releases: RefCell<Vec<(NsId, u64)>>,
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
    /// Last DDL-ticket generation MAINTAIN observed (ADR-0108 D1: a
    /// release wakes the pumps parked on the ticket).
    ddl_gen_seen: Cell<u64>,
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
    /// M4.5-S37 step 1 (`bench-diagnostics` only): the blind-overwrite
    /// ceiling arm — a plain SET whose only candidate is cold skips the
    /// verifying read and inserts as if absent. Unsound by construction
    /// (the cold record is orphaned); it bounds the cold read's cost.
    #[cfg(feature = "bench-diagnostics")]
    pub(crate) blind_overwrite_ceiling: Cell<bool>,
}

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
    /// produced, against the store `scope` names, at injected time `now`.
    fn on_execute(
        &mut self,
        cell: CellId,
        origin: ExecOrigin,
        scope: ExecScope,
        argv: &[&[u8]],
        reply: &[u8],
        now: Nanos,
    );
}

/// The store one applied command addressed (review of 2026-08-30,
/// F-L19-05 / ADR-0105): a replay model must apply the same argv to the
/// same store, so the apply seam names it. Mirrors `ConnCx::{db, ns}` as
/// they stood *when the command executed* — before a conn-state command
/// (`SELECT`, `INF.NS USE`) takes effect — and the `(db | ns)` an
/// `Apply`/`ApplyNs` leg carried on the owner side.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum ExecScope {
    /// Numbered database `db` of the default namespace.
    Db(u16),
    /// A named namespace.
    Ns(NsId),
    /// The connection's configured default namespace is unavailable
    /// (`--conn-default-ns` fail-closed state, M4.5-S40): data commands
    /// answered the typed refusal against no store.
    Unavailable,
}

impl ExecScope {
    fn of(cx: &ConnCx) -> ExecScope {
        match cx.ns {
            ConnNamespace::Named(ns) => ExecScope::Ns(ns),
            ConnNamespace::Default => ExecScope::Db(cx.db),
            ConnNamespace::RequiredUnavailable => ExecScope::Unavailable,
        }
    }
}

/// Where an applied command came from.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum ExecOrigin {
    /// A connection on this cell (slab slot, generation).
    Conn(u32, u32),
    /// A fabric `Apply` on behalf of the origin cell.
    Fabric(CellId),
}

/// Who composed an `Apply`'s argv (ADR-0115): a plane program — the move
/// legs, the pub/sub and DDL fabric vocabulary — or a client whose command
/// the plane forwards or scatters. Rides the codec's program mark; the
/// receiver executes `INTERNAL` rows and the pre-registry vocabulary only
/// under `Program`.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum ApplyOrigin {
    Client,
    Program,
}

/// Observer that observes nothing (the production default).
#[derive(Default, Debug)]
pub struct NoopObserver;

impl PlaneObserver for NoopObserver {
    #[inline]
    fn on_execute(
        &mut self,
        _: CellId,
        _: ExecOrigin,
        _: ExecScope,
        _: &[&[u8]],
        _: &[u8],
        _: Nanos,
    ) {
    }
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
    /// An accept-retry wheel key is pending (F-L11-02: at most one).
    accept_retry_armed: bool,
    /// Accepted fds are TCP sockets (`infinityd`, the e2e harness): the
    /// plane sets `tcp-keepalive` on them (ADR-0123 D3). The DST models
    /// no TCP stack and leaves this off.
    tcp_transport: bool,
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
    /// The graceful stop (ADR-0124 D2); `None` while serving.
    stop: Option<StopDrive>,
    /// Take a checkpoint before the final sync of a stop (D2 step 3).
    stop_checkpoint: bool,
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
    /// M4.5-S39d: the phase the previous step ran and the loop instant it
    /// ran at — the delta to the next step is that phase's time.
    last_step: Option<(RecoverPhase, Nanos)>,
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

/// Where a cell is in its graceful stop (ADR-0124 D2): `Serving` until
/// [`ServerPlane::request_stop`]; `Draining` while connections flush and
/// close; `Quiet` once none is live — the cell still applies peers' hops
/// and waits for the assembly's [`ServerPlane::finish_stop`] (every cell
/// quiet: no client-driven hop is in flight anywhere); then the stop
/// checkpoint publishes and the final sync lands; `Drained` once nothing
/// durable is in motion.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum StopPhase {
    Serving,
    Draining,
    Quiet,
    Drained,
}

/// The drain's own state (one `Option` check per MAINTAIN while serving).
struct StopDrive {
    phase: StopPhase,
    /// Every live connection has been marked `close_after_flush`.
    conns_marked: bool,
    /// The assembly saw every cell `Quiet` (`finish_stop`).
    finish: bool,
    /// The stop checkpoint's request epoch, once requested.
    ckpt_epoch: Option<u64>,
    final_sync_requested: bool,
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
        // ADR-0123 D5: the connection knobs before the first accept.
        let knobs = crate::config::conn_knobs(&node.config.borrow(), cells);
        ServerPlane {
            shared: Rc::new(Shared {
                cell,
                cells,
                router: SlotRouter::new_contiguous(cells),
                hasher: store.hasher(),
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
                accept_errors: Cell::new(0),
                nested_batch_ops_dropped: Cell::new(0),
                pubsub: RefCell::new(PubSubCell::new(cells)),
                pub_queue: RefCell::new(VecDeque::new()),
                pub_pump_active: Cell::new(false),
                knobs: Cell::new(knobs),
                parser_limits: Cell::new(ParserLimits::default()),
                durable: RefCell::new(None),
                tier: RefCell::new(None),
                ns_drop_releases: RefCell::new(Vec::new()),
                ns_applies: RefCell::new((0..cells).map(|_| VecDeque::new()).collect::<Vec<_>>()),
                ns_pump_active: RefCell::new(vec![false; usize::from(cells)]),
                pump_gated: RefCell::new(VecDeque::new()),
                control: RefCell::new(None),
                ddl_waiters: WaitList::new(),
                ckpt_waiters: WaitList::new(),
                ddl_epoch_seen: Cell::new(0),
                ddl_gen_seen: Cell::new(0),
                ckpt_pub_seen: Cell::new(0),
                loading: Cell::new(false),
                apply_prefetch: Cell::new(false),
                parse_prefetch: Cell::new(false),
                deasync_dispatch: Cell::new(false),
                #[cfg(feature = "bench-diagnostics")]
                blind_overwrite_ceiling: Cell::new(false),
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
            accept_retry_armed: false,
            tcp_transport: false,
            ckpt_epoch_seen: 0,
            early_fabric_flush: false,
            boot: None,
            boot_error: None,
            loading_board: None,
            memory_publish_in: 1,
            stop: None,
            stop_checkpoint: true,
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
            last_step: None,
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
        // Per-phase time (M4.5-S39d): the loop clock between consecutive
        // steps is the earlier step's phase — one injected instant per
        // iteration, never an ambient clock in cell code (L7).
        let phase = boot.rec.phase();
        if let Some((prev, at)) = boot.last_step.replace((phase, cx.now)) {
            boot.rec.credit_phase_time(prev, cx.now.0.saturating_sub(at.0));
        }
        if let Some(board) = &self.loading_board {
            board.slot(boot.cell_id).publish_phase(phase.code());
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
                        self.shared
                            .node
                            .recover_segment_residue_stops
                            .set(stats.segment_residue_stops);
                        self.shared
                            .node
                            .recover_recycled_residue_slacks
                            .set(stats.recycled_residue_slacks);
                        self.shared
                            .node
                            .recover_stale_residue_slacks
                            .set(stats.stale_residue_slacks);
                        self.shared.node.recover_phases.set(stats.phases);
                        self.shared.node.recover_stale_files_removed.set(stats.stale_files_removed);
                        self.shared
                            .node
                            .recover_records
                            .set(stats.ckpt_records + stats.records_applied);
                        self.shared.node.recover_replay_records.set(stats.records_applied);
                        if let Some(board) = &self.loading_board {
                            let slot = board.slot(boot.cell_id);
                            slot.mark_ready(
                                stats.ckpt_records + stats.records_applied,
                                stats.torn_truncated_at,
                                crate::control::RecoveredResidue {
                                    segment_residue_stops: stats.segment_residue_stops,
                                    recycled_residue_slacks: stats.recycled_residue_slacks,
                                    stale_residue_slacks: stats.stale_residue_slacks,
                                },
                                stats.phases,
                                stats.records_skipped_unknown_ns,
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
            self.shared.hasher.identity(),
            recovered.map(|m| crate::ckpt::PendingManifest {
                ckpt_id: m.ckpt_id,
                begin_lsn: m.begin_lsn,
            }),
        );
        *self.shared.durable.borrow_mut() = Some(DurableCell::new(cfg, rotor, ckpt, manifest));
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
        /// The plane's flush admission: a short borrow of the durable
        /// cell per offer (the tier borrow is held by the caller; the
        /// durable cell is not — no nested `RefCell` conflict).
        struct DurableFlushAdmission<'a, F: SegmentFs> {
            durable: &'a RefCell<Option<DurableCell<F>>>,
        }
        impl<F: SegmentFs> crate::tier_cell::FlushAdmission for DurableFlushAdmission<'_, F> {
            fn admit(&mut self, bytes: u64, ops: u64) -> bool {
                match self.durable.borrow_mut().as_mut() {
                    Some(cell) => {
                        cell.admit_background(IoClass::TierFlush, bytes, ops) == Admission::Granted
                    }
                    None => true,
                }
            }
            fn refund(&mut self, bytes: u64, ops: u64) {
                if let Some(cell) = self.durable.borrow_mut().as_mut() {
                    cell.refund_background(IoClass::TierFlush, bytes, ops);
                }
            }
        }
        let mut compact_reads: Vec<crate::tier_cell::CompactRead> = Vec::new();
        let mut shadow_reads: Vec<(NsId, inf_store::ShadowRead)> = Vec::new();
        let mut flush_ops: Vec<IoOp> = Vec::new();
        let cold = {
            let mut tier_slot = self.shared.tier.borrow_mut();
            let Some(tier) = tier_slot.as_mut() else { return };
            tier.sync_namespaces(
                &self.shared.store.borrow(),
                &mut self.shared.ns_drop_releases.borrow_mut(),
            );
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
            // ADR-0088 D5: flush rounds offer their slice to the durable
            // cell's device budget (the tier cell has no budget of its
            // own — one owner per fact). No durable plane ⇒ admit all.
            let mut admission = DurableFlushAdmission { durable: &self.shared.durable };
            for at in 0..tier.namespaces.len() {
                match tier.maintain_ns(
                    &mut ks,
                    at,
                    durable_mark,
                    transition_idle,
                    now_us,
                    &mut flush_ops,
                    &mut admission,
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
            // M4.5-S37 (ADR-0093 D4): the shadow reconciler's reads —
            // oldest winner first, bounded per table, Foreground once
            // the pinned suffix crosses half its cap.
            for t in &tier.namespaces {
                if let Some(table) = ks.tiered_store_mut(t.ns) {
                    let reads = table.shadow_work(inf_store::SHADOW_READS_IN_FLIGHT);
                    shadow_reads.extend(reads.into_iter().map(|read| (t.ns, read)));
                }
            }
            // ADR-0100 D5: a dropped namespace's files unlink only once
            // the catalog swap carrying the drop is durable.
            let control = self.shared.control.borrow().clone();
            units += tier.maintain_teardown(|epoch| {
                control.as_ref().is_none_or(|control| control.persisted(epoch))
            });
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
                stats.rounds_deferred,
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
        for (ns, read) in shadow_reads {
            let shared = Rc::clone(&self.shared);
            let _ = cx.executor.poll_immediate(shadow_pump(shared, ns, read));
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
            // ADR-0088 D2/D5: maintain-class reads consult the durable
            // cell's budget; foreground reads are charged, never refused.
            let durable = &self.shared.durable;
            cold.drain_budgeted(
                |class, bytes| match durable.borrow_mut().as_mut() {
                    Some(cell) => match class {
                        inf_runtime::ReadClass::Foreground => {
                            cell.charge_foreground(IoClass::ColdReadForeground, bytes, 1);
                            true
                        }
                        inf_runtime::ReadClass::Maintain => {
                            cell.admit_background(IoClass::ColdReadMaintain, bytes, 1)
                                == Admission::Granted
                        }
                    },
                    None => true,
                },
                |class, unused_bytes, unused_ops| {
                    if let Some(cell) = durable.borrow_mut().as_mut() {
                        let io_class = match class {
                            inf_runtime::ReadClass::Foreground => IoClass::ColdReadForeground,
                            inf_runtime::ReadClass::Maintain => IoClass::ColdReadMaintain,
                        };
                        cell.refund_background_or_foreground(io_class, unused_bytes, unused_ops);
                    }
                },
                |op| cx.push(op),
            );
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

    /// Checkpoint `(completed, aborted)` for the simulator's oracles
    /// (M4.5-S36) — used only between scheduler steps.
    pub fn ckpt_stats_for_sim(&self) -> (u64, u64) {
        self.shared.durable.borrow().as_ref().map_or((0, 0), |cell| {
            let s = cell.ckpt_stats();
            (s.completed, s.aborted)
        })
    }

    /// Wires this plane's slot of the doorbell-wakeup park board (the same
    /// `Arc` goes to every cell's fabric via `CellFabric::set_wakeups`).
    pub fn set_park_flags(&mut self, flags: Arc<Vec<AtomicBool>>) {
        self.park_flags = Some(flags);
    }

    /// Enables the M2.5-S21 early fabric publish (A/B lever): remote ops
    /// staged during EXECUTE are published at the head of MAINTAIN.
    /// Accepted fds are real TCP sockets: apply `tcp-keepalive` at accept
    /// (ADR-0123 D3). Off by default (the DST's fds are not sockets).
    pub fn set_tcp_transport(&mut self, on: bool) {
        self.tcp_transport = on;
    }

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

    /// M4.5-S37 step 1: arms the blind-overwrite ceiling instrument
    /// (`bench-diagnostics` builds only — an unsound measurement arm,
    /// never a serving configuration).
    #[cfg(feature = "bench-diagnostics")]
    pub fn set_blind_overwrite_ceiling(&mut self, on: bool) {
        self.shared.blind_overwrite_ceiling.set(on);
    }

    /// Live connections (tests, stats).
    /// Begins the graceful stop (ADR-0124 D2); idempotent. From the next
    /// MAINTAIN on, accepts are closed unanswered, every connection
    /// flushes and closes, a durable cell takes its stop checkpoint and
    /// final sync, and [`stop_phase`](Self::stop_phase) reaches
    /// `Drained`.
    pub fn request_stop(&mut self) {
        if self.stop.is_none() {
            self.stop = Some(StopDrive {
                phase: StopPhase::Draining,
                conns_marked: false,
                finish: false,
                ckpt_epoch: None,
                final_sync_requested: false,
            });
        }
    }

    /// The assembly's word that every cell is `Quiet` (ADR-0124 D3): no
    /// client-driven hop is in flight anywhere, so this cell's stop
    /// checkpoint and final sync capture everything. Idempotent; a no-op
    /// before [`request_stop`](Self::request_stop).
    pub fn finish_stop(&mut self) {
        if let Some(stop) = self.stop.as_mut() {
            stop.finish = true;
        }
    }

    /// Where the cell is in its stop.
    #[must_use]
    pub fn stop_phase(&self) -> StopPhase {
        self.stop.as_ref().map_or(StopPhase::Serving, |s| s.phase)
    }

    /// Whether a stop takes a checkpoint before its final sync (on by
    /// default — the next boot then replays nothing; `infinityd
    /// --shutdown-checkpoint off`).
    pub fn set_stop_checkpoint(&mut self, on: bool) {
        self.stop_checkpoint = on;
    }

    /// One MAINTAIN step of the drain (ADR-0124 D2). Returns without
    /// touching anything while serving.
    fn drive_stop(&mut self) {
        let Some(stop) = self.stop.as_mut() else { return };
        if stop.phase == StopPhase::Drained {
            return;
        }
        // Step 2: connections flush and close (QUIT's path — bytes read
        // after the mark are dropped; a command in flight finishes).
        if !stop.conns_marked {
            stop.conns_marked = true;
            self.shared.conns.borrow_mut().for_each_mut(|conn| conn.close_after_flush = true);
        }
        if self.shared.conns.borrow().live > 0 {
            return;
        }
        stop.phase = StopPhase::Quiet;
        if !stop.finish {
            return;
        }
        // Steps 3–4: a durable cell publishes its stop checkpoint, then
        // its final sync lands. A cell still in boot recovery has no
        // durable plane yet: it acked nothing, and its disk is the
        // previous life's — Drained.
        let mut durable = self.shared.durable.borrow_mut();
        if let Some(cell) = durable.as_mut() {
            let control = self.shared.control.borrow();
            if self.stop_checkpoint
                && let Some(control) = control.as_ref()
            {
                let me = self.shared.cell.0;
                let epoch = *stop.ckpt_epoch.get_or_insert_with(|| control.request_ckpt_cell(me));
                if control.ckpt_board().slot(me).published() < epoch {
                    return;
                }
            }
            if !stop.final_sync_requested {
                stop.final_sync_requested = true;
                cell.request_final_sync();
                return;
            }
            if !cell.quiescent() {
                return;
            }
        }
        stop.phase = StopPhase::Drained;
    }

    pub fn connections(&self) -> usize {
        self.shared.conns.borrow().live
    }

    /// Outstanding async work: pending fabric replies + credit waiters.
    /// Quiescence (sim) means zero.
    pub fn suspended(&self) -> usize {
        self.shared.gate.pending() + self.shared.credit_waiters.waiting()
    }

    /// This cell's fabric counters (the DST's drain-fairness oracle,
    /// F-L12-02; tooling — never the data plane).
    pub fn fabric_stats(&self) -> inf_fabric::FabricStats {
        self.shared.fabric.borrow().stats()
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

    /// Every live entry on this cell at `now` — `(scope, key, value,
    /// expiry deadline in internal ms)` — across the numbered dbs and the
    /// materialized memory / flat-durable named namespaces (review of
    /// 2026-08-30, F-L19-06: the sim's content oracle compares values and
    /// deadlines, not key sets or counts). Expired-but-unreaped records are
    /// reaped, never emitted, so a model folded at the same `now` agrees
    /// without an expiry-equalization pass. Tiered namespaces are skipped
    /// — their post-images live partly on the device, and the tiered DST's
    /// phase oracles own them — and the skip count is returned so a caller
    /// cannot mistake a skipped namespace for an empty one.
    pub fn fold_live_entries(
        &self,
        now: Nanos,
        emit: impl FnMut(ExecScope, &[u8], &[u8], Option<u64>),
    ) -> usize {
        fold_live_entries(&mut self.shared.store.borrow_mut(), now, emit)
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

    /// Refuses an accepted socket (ADR-0123 D1): counted in
    /// `rejected_connections`, Redis's error frame sent on the reserved
    /// top slot with the fd in the token's generation, and closed on that
    /// send's completion — never a `Close` queued beside its `Send`, which
    /// io_uring may cancel. A dry send pool closes at once, silently.
    fn refuse_accept(&self, cx: &mut LoopCx<'_>, fd: RawFd) {
        let node = &self.shared.node;
        node.rejected_connections.set(node.rejected_connections.get() + 1);
        let frame = MAXCLIENTS_REFUSAL;
        match (u32::try_from(fd), cx.pool.try_lease(LeaseKind::Send)) {
            (Ok(generation), Some(buf)) if frame.len() <= cx.pool.buf_size() => {
                cx.pool.bytes_mut(buf)[..frame.len()].copy_from_slice(frame);
                cx.push(IoOp::Send {
                    fd,
                    buf,
                    len: frame.len() as u32,
                    token: CompletionToken::new(TokenClass::Send, CONN_SLOT_CAP, generation),
                });
            }
            (_, lease) => {
                if let Some(buf) = lease {
                    cx.pool.release(buf);
                }
                let refused = ConnKey { slot: CONN_SLOT_CAP, generation: 0 };
                cx.push(IoOp::Close { fd, token: Self::token(TokenClass::Close, refused) });
            }
        }
    }

    /// Closes an accepted socket without a frame (a stop drain), counted
    /// with the refusals.
    fn close_unadmitted(&self, cx: &mut LoopCx<'_>, fd: RawFd) {
        let node = &self.shared.node;
        node.rejected_connections.set(node.rejected_connections.get() + 1);
        let refused = ConnKey { slot: CONN_SLOT_CAP, generation: 0 };
        cx.push(IoOp::Close { fd, token: Self::token(TokenClass::Close, refused) });
    }

    /// Closes the fd a reserved-slot send named (see `refuse_accept`).
    fn close_refused(&self, cx: &mut LoopCx<'_>, token: CompletionToken) {
        // `refuse_accept` stored a non-negative fd (`u32::try_from` passed),
        // so the cast is exact.
        let fd = token.generation() as RawFd;
        let refused = ConnKey { slot: CONN_SLOT_CAP, generation: 0 };
        cx.push(IoOp::Close { fd, token: Self::token(TokenClass::Close, refused) });
    }

    /// Spawn the per-connection windowed pump with its first command.
    fn spawn_pump(&self, cx: &mut LoopCx<'_>, key: ConnKey, first: OwnedCmd) {
        let shared = Rc::clone(&self.shared);
        let _ = cx.executor.poll_immediate(pump(shared, key, first));
    }
}

impl<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static> ServerPlane<O, F> {
    /// The namespace a fresh connection starts in (M4.5-S40,
    /// `--conn-default-ns`): resolved by name against the catalog at
    /// accept time. An unknown name or a topic becomes a fail-closed
    /// connection state; only inspection and explicit namespace recovery
    /// commands run until `SELECT` or `INF.NS USE`. One `RefCell` borrow
    /// per accept.
    fn conn_default_ns(&self) -> ConnNamespace {
        let name = self.shared.node.conn_default_ns.borrow();
        let Some(name) = name.as_deref() else { return ConnNamespace::Default };
        let store = self.shared.store.borrow();
        resolve_conn_default(store.ns_get(name).map(|spec| (spec.id, spec.mode)))
    }
}

fn resolve_conn_default(spec: Option<(NsId, NsMode)>) -> ConnNamespace {
    match spec {
        Some((_, NsMode::Topic)) | None => ConnNamespace::RequiredUnavailable,
        Some((id, NsMode::Memory | NsMode::Durable)) => ConnNamespace::Named(id),
    }
}

#[cfg(test)]
mod conn_default_tests {
    use super::{ConnNamespace, NsId, NsMode, resolve_conn_default};

    #[test]
    fn configured_default_resolves_named_modes_and_refuses_missing_or_topic() {
        let id = NsId(42);
        assert_eq!(resolve_conn_default(None), ConnNamespace::RequiredUnavailable);
        assert_eq!(
            resolve_conn_default(Some((id, NsMode::Topic))),
            ConnNamespace::RequiredUnavailable
        );
        assert_eq!(resolve_conn_default(Some((id, NsMode::Memory))), ConnNamespace::Named(id));
        assert_eq!(resolve_conn_default(Some((id, NsMode::Durable))), ConnNamespace::Named(id));
    }
}

#[cfg(test)]
#[path = "move_tests.rs"]
mod move_tests;
