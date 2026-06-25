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
//!   single-owner commands ship as `Op::Apply { namespace, cmd: protocol,
//!   args: argv }` and return the owner's raw RESP reply (`Outcome::Bytes`) —
//!   byte-exact by construction. `DEL`/`EXISTS` split per key and aggregate
//!   typed `Outcome::Int` replies.
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
use inf_log::{
    CheckpointId, CheckpointRef, FrameMeta, Lsn, NamespaceId, RecoveryManifest, SegmentId,
    scan_segment_names,
};
use inf_runtime::GroupClass;
use inf_runtime::gate::KeyedGate;
use inf_runtime::{
    CellPlane, Completion, CompletionResult, CompletionToken, FabricGate, FileSyncMode, GateWait,
    IoOp, LoopCx, RawFd, TokenClass, WaitList, WatermarkGate,
};
use inf_store::{
    EvictBudget, ExpiryBudget, Keyspace, NsCatalog, NsFsyncPolicy, NsMode, SlotRouter,
};
use inf_wire::{
    ArgvRef, CmdFlags, CommandId, CommandMeta, ConnParser, Parsed, ParserLimits, Protocol,
    RespWriter, arity_ok, extract_keys, lookup,
};

use crate::checkpoint::{
    CheckpointKeyspacePublishError, CheckpointKeyspaceSnapshotConfig,
    EncodedCheckpointKeyspaceSnapshotParts, LiveCheckpointPublishError, LiveCheckpointPublishEvent,
    LiveCheckpointPublisher, encode_checkpoint_keyspace_snapshot_image_from_parts,
    encode_checkpoint_keyspace_snapshot_parts,
};
use crate::durability::DurabilityCell;
use crate::exec::{
    ConnCx, ConnNamespace, NodeInfo, execute, execute_durable, execute_slices, stall_request,
    wall_ms,
};
use crate::log_maintenance::{
    LogSegmentMaintenance, LogSegmentMaintenanceError, LogSegmentMaintenanceEvent,
};
use crate::log_writer::{LogWriteCompletion, LogWriteIo, LogWriteIoError};
use crate::ns_catalog::{
    NamespaceCatalogLivePublishEvent, NamespaceCatalogLivePublisher, NamespaceCatalogPublishError,
};
use crate::pubsub::{self, PubSubCell, SubKind};

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
/// Live namespace DDL publishes are bounded and FIFO. Overflow is fail-stop:
/// after a namespace mutation has applied, continuing without publishing the
/// restart catalog would violate the ack contract.
const NS_CATALOG_PUBLISH_QUEUE_MAX: usize = 64;
/// M2-S20 v1 accepts one checkpoint request per cell. More would need
/// control-plane coalescing and per-cell progress reporting; a bounded busy
/// error is preferable to an implicit unbounded save backlog.
const CHECKPOINT_PUBLISH_QUEUE_MAX: usize = 1;
/// Owner-side durable fabric replies are bounded by fabric credits in normal
/// operation; this cap turns a broken pressure path into fail-stop instead of
/// unbounded memory.
const OWNER_DURABLE_REPLY_QUEUE_MAX: usize = 4096;
/// Owner-side remote checkpoint WAIT replies are bounded by fabric credits and
/// by the one-checkpoint-per-cell queue. The cap catches any broken pressure
/// path before it becomes unbounded memory.
const OWNER_CHECKPOINT_REPLY_QUEUE_MAX: usize = 4096;
/// Cell-local timer key for the M2 `everysec` fsync policy.
const LOG_EVERYSEC_TIMER_KEY: u64 = u64::MAX - 17;
const LOG_EVERYSEC_INTERVAL: Nanos = Nanos::from_secs(1);
/// POSIX ENOSPC. Kept local to avoid moving libc into the server-plane API.
const ENOSPC_ERRNO: i32 = 28;
const DURABLE_PREALLOCATE_ENOSPC_ERROR: &str =
    "ERR durable write rejected: log preallocation failed with ENOSPC";
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

struct Shared<O: PlaneObserver + 'static> {
    cell: CellId,
    cells: u16,
    router: SlotRouter,
    /// Forces every key local — the cross-cell penalty A/B leg (§6 gate).
    route_local_only: bool,
    /// `DEBUG SLEEP` cell stall: connection parse/respond pause until this
    /// injected-clock instant (fabric service continues — deadlock safety).
    stall_until: Cell<Nanos>,
    store: RefCell<Keyspace>,
    durability: RefCell<DurabilityCell>,
    durable_write_refusal: Cell<Option<DurableWriteRefusal>>,
    log_writer: RefCell<Option<LogWriteIo>>,
    durability_gate: WatermarkGate,
    durability_assignment_gate: KeyedGate<u64, u64>,
    durability_assignment_tickets: RefCell<VecDeque<u64>>,
    durability_assignment_next_ticket: Cell<u64>,
    ns_catalog_publish_gate: KeyedGate<u64, ()>,
    ns_catalog_publish_requests: RefCell<VecDeque<NamespaceCatalogPublishRequest>>,
    ns_catalog_publish_next_ticket: Cell<u64>,
    ns_catalog_publish_enabled: Cell<bool>,
    checkpoint_publish_gate: KeyedGate<u64, ()>,
    checkpoint_publish_requests: RefCell<VecDeque<CheckpointPublishRequest>>,
    checkpoint_publish_next_ticket: Cell<u64>,
    checkpoint_publish_enabled: Cell<bool>,
    checkpoint_publish_inflight: Cell<bool>,
    #[cfg(test)]
    durability_default_policy: Cell<Option<NsFsyncPolicy>>,
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
    /// Pub/sub registries (M1-S10): local subscriber lists, owner-side
    /// per-cell counts, the replicated pattern index.
    pubsub: RefCell<PubSubCell<ConnKey>>,
    /// Fabric-origin PUBLISHes awaiting this cell's owner pump (FIFO — the
    /// queue preserves per-publisher delivery order across fan-outs).
    pub_queue: RefCell<VecDeque<OwnerPub>>,
    pub_pump_active: Cell<bool>,
    /// Fabric-origin durable writes whose owner reply must wait for this
    /// cell's fsync watermark before the reply credit is returned.
    owner_durable_replies: RefCell<VecDeque<OwnerDurableReply>>,
    owner_durable_pump_active: Cell<bool>,
    /// Fabric-origin `INF.CKPT WAIT` requests whose owner reply must wait for
    /// this cell's MANIFEST publish gate before the reply credit is returned.
    owner_checkpoint_replies: RefCell<VecDeque<OwnerCheckpointReply>>,
    owner_checkpoint_pump_active: Cell<bool>,
    /// Parsed `client-output-buffer-limit pubsub` `(hard, soft, soft_ms)`
    /// (M1-S11); refreshed by the MAINTAIN config sweep. Zeros disable.
    cob_pubsub: Cell<(u64, u64, u64)>,
}

/// One fabric-origin PUBLISH parked at the owner cell.
struct OwnerPub {
    origin: CellId,
    token: FabricToken,
    channel: Vec<u8>,
    payload: Vec<u8>,
}

struct OwnerDurableReply {
    origin: CellId,
    token: FabricToken,
    assignment: GateWait<u64, u64>,
    body: OwnerDurableReplyBody,
}

struct OwnerCheckpointReply {
    origin: CellId,
    token: FabricToken,
    waiter: GateWait<u64, ()>,
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
struct DurableNamespace {
    id: NamespaceId,
    fsync: NsFsyncPolicy,
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum DurableWriteRefusal {
    LogPreallocateEnospc { segment: SegmentId, errno: i32 },
}

impl DurableWriteRefusal {
    #[inline]
    const fn message(self) -> &'static str {
        match self {
            DurableWriteRefusal::LogPreallocateEnospc { segment, errno } => {
                let _ = (segment, errno);
                DURABLE_PREALLOCATE_ENOSPC_ERROR
            }
        }
    }
}

enum OwnerDurableReplyBody {
    Bytes(Vec<u8>),
    Int(i64),
}

struct NamespaceCatalogPublishRequest {
    ticket: u64,
    snapshot: NsCatalog,
}

struct CheckpointPublishRequest {
    ticket: Option<u64>,
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum CheckpointQueueError {
    PublisherUnavailable,
    Busy,
}

impl CheckpointQueueError {
    const fn message(self) -> &'static str {
        match self {
            CheckpointQueueError::PublisherUnavailable => {
                "ERR checkpoint publisher is not installed"
            }
            CheckpointQueueError::Busy => "ERR background save already in progress",
        }
    }
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
        namespace: ConnNamespace,
        out: &mut Vec<u8>,
    ) {
        let before = out.len();
        let mut cx = ConnCx {
            proto,
            id,
            namespace,
            sub_channels: Vec::new(),
            sub_patterns: Vec::new(),
            node: Rc::clone(&self.node),
            close_requested: Cell::new(false),
        };
        let now = self.now.get();
        execute_slices(argv, &mut self.store.borrow_mut(), &mut cx, now, out);
        self.observer.borrow_mut().on_execute(self.cell, origin, argv, &out[before..], now);
    }

    /// Executes a local durable write and returns a frame-assignment waiter
    /// only when the command staged bytes. The final LSN is assigned by LOG,
    /// so `seal_log` completes this ticket with the frame-end watermark key.
    fn execute_durable_always_into(
        &self,
        origin: ExecOrigin,
        argv: &[&[u8]],
        proto: Protocol,
        namespace_selected: ConnNamespace,
        namespace: NamespaceId,
        out: &mut Vec<u8>,
    ) -> Option<GateWait<u64, u64>> {
        if self.refuse_durable_write_into(origin, argv, proto, out) {
            return None;
        }
        let before = out.len();
        let before_staged = self.durability.borrow().log_staging_bytes();
        let mut cx = ConnCx {
            proto,
            id: 0,
            namespace: namespace_selected,
            sub_channels: Vec::new(),
            sub_patterns: Vec::new(),
            node: Rc::clone(&self.node),
            close_requested: Cell::new(false),
        };
        let now = self.now.get();
        {
            let mut store = self.store.borrow_mut();
            let mut durability = self.durability.borrow_mut();
            execute_durable(argv, &mut store, &mut durability, namespace, &mut cx, now, out);
        }
        self.observer.borrow_mut().on_execute(self.cell, origin, argv, &out[before..], now);
        let after_staged = self.durability.borrow().log_staging_bytes();
        if after_staged > before_staged {
            Some(self.register_durability_assignment())
        } else {
            None
        }
    }

    fn execute_durable_everysec_into(
        &self,
        origin: ExecOrigin,
        argv: &[&[u8]],
        proto: Protocol,
        namespace_selected: ConnNamespace,
        namespace: NamespaceId,
        out: &mut Vec<u8>,
    ) {
        if self.refuse_durable_write_into(origin, argv, proto, out) {
            return;
        }
        let before = out.len();
        let mut cx = ConnCx {
            proto,
            id: 0,
            namespace: namespace_selected,
            sub_channels: Vec::new(),
            sub_patterns: Vec::new(),
            node: Rc::clone(&self.node),
            close_requested: Cell::new(false),
        };
        let now = self.now.get();
        {
            let mut store = self.store.borrow_mut();
            let mut durability = self.durability.borrow_mut();
            execute_durable(argv, &mut store, &mut durability, namespace, &mut cx, now, out);
        }
        self.observer.borrow_mut().on_execute(self.cell, origin, argv, &out[before..], now);
    }

    fn refuse_durable_write_into(
        &self,
        origin: ExecOrigin,
        argv: &[&[u8]],
        proto: Protocol,
        out: &mut Vec<u8>,
    ) -> bool {
        let Some(refusal) = self.durable_write_refusal.get() else {
            return false;
        };
        let before = out.len();
        RespWriter::new(out, proto).error(refusal.message());
        self.observer.borrow_mut().on_execute(
            self.cell,
            origin,
            argv,
            &out[before..],
            self.now.get(),
        );
        true
    }

    fn durable_namespace_policy(
        &self,
        meta: &'static CommandMeta,
        namespace: ConnNamespace,
    ) -> Option<DurableNamespace> {
        if !meta.flags.contains(CmdFlags::WRITE) {
            return None;
        }
        #[cfg(test)]
        {
            if let Some(fsync) = self.durability_default_policy.get() {
                return Some(DurableNamespace { id: NamespaceId::new(namespace.id()), fsync });
            }
        }
        let ConnNamespace::Named(id) = namespace else { return None };
        let store = self.store.borrow();
        let spec = store.ns_get_by_id(id)?;
        let fsync = spec.fsync?;
        (spec.mode == NsMode::Durable)
            .then_some(DurableNamespace { id: NamespaceId::new(id.get()), fsync })
    }

    fn register_durability_assignment(&self) -> GateWait<u64, u64> {
        let ticket = self.durability_assignment_next_ticket.get();
        self.durability_assignment_next_ticket
            .set(ticket.checked_add(1).expect("durability assignment tickets exhausted"));
        let waiter = self.durability_assignment_gate.waiter(ticket);
        self.durability_assignment_tickets.borrow_mut().push_back(ticket);
        waiter
    }

    fn complete_durability_assignments(&self, meta: FrameMeta) -> usize {
        let watermark_key = log_watermark_key(meta.frame_end());
        let mut completed = 0;
        let tickets: Vec<_> = self.durability_assignment_tickets.borrow_mut().drain(..).collect();
        for ticket in tickets {
            assert!(
                self.durability_assignment_gate.complete(ticket, watermark_key),
                "durability assignment ticket had no waiter"
            );
            completed += 1;
        }
        completed
    }

    fn push_owner_durable_reply(&self, reply: OwnerDurableReply) {
        let mut queue = self.owner_durable_replies.borrow_mut();
        assert!(
            queue.len() < OWNER_DURABLE_REPLY_QUEUE_MAX,
            "owner durable reply queue exceeded {OWNER_DURABLE_REPLY_QUEUE_MAX} entries"
        );
        queue.push_back(reply);
    }

    fn push_owner_checkpoint_reply(&self, reply: OwnerCheckpointReply) {
        let mut queue = self.owner_checkpoint_replies.borrow_mut();
        assert!(
            queue.len() < OWNER_CHECKPOINT_REPLY_QUEUE_MAX,
            "owner checkpoint reply queue exceeded {OWNER_CHECKPOINT_REPLY_QUEUE_MAX} entries"
        );
        queue.push_back(reply);
    }

    fn queue_namespace_catalog_publish(&self) -> GateWait<u64, ()> {
        assert!(
            self.ns_catalog_publish_enabled.get(),
            "namespace catalog publish queued without a live publisher"
        );
        let ticket = self.ns_catalog_publish_next_ticket.get();
        self.ns_catalog_publish_next_ticket
            .set(ticket.checked_add(1).expect("namespace catalog publish tickets exhausted"));
        let waiter = self.ns_catalog_publish_gate.waiter(ticket);
        let snapshot = self.store.borrow().ns_catalog_snapshot();
        let mut requests = self.ns_catalog_publish_requests.borrow_mut();
        assert!(
            requests.len() < NS_CATALOG_PUBLISH_QUEUE_MAX,
            "namespace catalog publish queue exceeded {NS_CATALOG_PUBLISH_QUEUE_MAX} entries"
        );
        requests.push_back(NamespaceCatalogPublishRequest { ticket, snapshot });
        waiter
    }

    fn complete_namespace_catalog_publish(&self, ticket: u64) {
        assert!(
            self.ns_catalog_publish_gate.complete(ticket, ()),
            "namespace catalog publish ticket had no waiter"
        );
    }

    fn try_queue_checkpoint_publish(
        &self,
        wait: bool,
    ) -> Result<Option<GateWait<u64, ()>>, CheckpointQueueError> {
        if !self.checkpoint_publish_enabled.get() {
            return Err(CheckpointQueueError::PublisherUnavailable);
        }
        if self.checkpoint_publish_inflight.get() {
            return Err(CheckpointQueueError::Busy);
        }
        let mut requests = self.checkpoint_publish_requests.borrow_mut();
        if requests.len() >= CHECKPOINT_PUBLISH_QUEUE_MAX {
            return Err(CheckpointQueueError::Busy);
        }
        let (ticket, waiter) = if wait {
            let ticket = self.checkpoint_publish_next_ticket.get();
            self.checkpoint_publish_next_ticket
                .set(ticket.checked_add(1).expect("checkpoint publish tickets exhausted"));
            (Some(ticket), Some(self.checkpoint_publish_gate.waiter(ticket)))
        } else {
            (None, None)
        };
        requests.push_back(CheckpointPublishRequest { ticket });
        self.checkpoint_publish_inflight.set(true);
        self.node.checkpoint_in_progress.set(1);
        Ok(waiter)
    }

    fn complete_checkpoint_publish(&self, ticket: Option<u64>, now_ms: u64) {
        if let Some(ticket) = ticket {
            assert!(
                self.checkpoint_publish_gate.complete(ticket, ()),
                "checkpoint publish ticket had no waiter"
            );
        }
        self.checkpoint_publish_inflight.set(false);
        self.node.checkpoint_in_progress.set(0);
        self.node.last_checkpoint_unix_ms.set(now_ms);
    }

    fn advance_durability_watermark(&self, meta: FrameMeta) -> usize {
        self.durability_gate.advance(log_watermark_key(meta.frame_end()))
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
    fn apply_counted(
        &self,
        origin: ExecOrigin,
        name: &[u8],
        key: &[u8],
        namespace: ConnNamespace,
    ) -> i64 {
        let now = self.now.get();
        let del = name.eq_ignore_ascii_case(b"DEL") || name.eq_ignore_ascii_case(b"UNLINK");
        let hit = {
            let mut ks = self.store.borrow_mut();
            let store = match namespace {
                ConnNamespace::Default(db) => ks.db_mut(usize::from(db)),
                ConnNamespace::Named(id) => match ks.named_db_mut(id) {
                    Some(store) => store,
                    None => return 0,
                },
            };
            if del { store.del(key, now) } else { store.exists(key, now) }
        };
        let mut reply = Vec::new();
        RespWriter::new(&mut reply, Protocol::Resp2).int(i64::from(hit));
        self.observer.borrow_mut().on_execute(self.cell, origin, &[name, key], &reply, now);
        i64::from(hit)
    }

    /// Typed DBSIZE apply (scatter contribution, M1-S02; per selected db).
    fn apply_dbsize(&self, origin: ExecOrigin, namespace: ConnNamespace) -> i64 {
        let now = self.now.get();
        let len = {
            let mut ks = self.store.borrow_mut();
            match namespace {
                ConnNamespace::Default(db) => ks.db_mut(usize::from(db)).len() as i64,
                ConnNamespace::Named(id) => {
                    ks.named_db_mut(id).map_or(0, |store| store.len() as i64)
                }
            }
        };
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
    /// Reusable LOG-step scratch. `LogWriteIo` emits at most one op today;
    /// the vector remains owned here so `LoopCx` can keep its queue private.
    log_ops: Vec<IoOp>,
    log_segment_maintenance: Option<LogSegmentMaintenance>,
    /// Reusable namespace-catalog publish scratch. Live DDL publish emits one
    /// backend op per state-machine step and never blocks in the cell.
    ns_catalog_ops: Vec<IoOp>,
    ns_catalog_publisher: Option<NamespaceCatalogLivePublisher>,
    ns_catalog_active_ticket: Option<u64>,
    /// Reusable checkpoint publish scratch. M2-S20 live checkpoint publishes
    /// the checkpoint image and MANIFEST through one backend op per step.
    checkpoint_ops: Vec<IoOp>,
    checkpoint_publisher: Option<LiveCheckpointPublisher>,
    checkpoint_active: Option<ActiveCheckpointPublish>,
    checkpoint_next_id: Option<CheckpointId>,
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
    everysec_timer_armed: bool,
}

enum ActiveCheckpointPublish {
    AwaitLog {
        ticket: Option<u64>,
        checkpoint_id: CheckpointId,
        parts: EncodedCheckpointKeyspaceSnapshotParts,
    },
    AwaitDurable {
        ticket: Option<u64>,
        checkpoint: CheckpointRef,
        parts: EncodedCheckpointKeyspaceSnapshotParts,
        watermark_key: u64,
    },
    Publishing {
        ticket: Option<u64>,
        checkpoint: CheckpointRef,
    },
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

enum OwnerApplyReply {
    Immediate(StagedReply),
    Deferred,
}

struct OwnerDurableApply<'a, 'b, O: PlaneObserver + 'static> {
    shared: &'a Shared<O>,
    from: CellId,
    token: FabricToken,
    argv: &'b [&'b [u8]],
    proto: Protocol,
    selected: ConnNamespace,
    namespace: NamespaceId,
    counted: bool,
    scratch: &'a mut Vec<u8>,
}

fn resp_int(bytes: &[u8]) -> Option<i64> {
    if bytes.first().copied() != Some(b':') {
        return None;
    }
    if !bytes.ends_with(b"\r\n") {
        return None;
    }
    crate::exec::parse_i64(&bytes[1..bytes.len() - 2]).ok()
}

fn push_scratch_bytes(scratch: &mut Vec<u8>, bytes: &[u8]) -> StagedReply {
    let start = scratch.len();
    scratch.extend_from_slice(bytes);
    StagedReply::Bytes(start, scratch.len())
}

fn push_scratch_simple(scratch: &mut Vec<u8>, proto: Protocol, text: &str) -> StagedReply {
    let start = scratch.len();
    RespWriter::new(scratch, proto).simple(text);
    StagedReply::Bytes(start, scratch.len())
}

fn push_scratch_error(scratch: &mut Vec<u8>, proto: Protocol, text: &str) -> StagedReply {
    let start = scratch.len();
    RespWriter::new(scratch, proto).error(text);
    StagedReply::Bytes(start, scratch.len())
}

fn execute_durable_apply_reply<O: PlaneObserver + 'static>(
    ctx: OwnerDurableApply<'_, '_, O>,
) -> OwnerApplyReply {
    let mut reply = ctx.shared.take_reply_buf();
    let assignment = ctx.shared.execute_durable_always_into(
        ExecOrigin::Fabric(ctx.from),
        ctx.argv,
        ctx.proto,
        ctx.selected,
        ctx.namespace,
        &mut reply,
    );
    if ctx.counted {
        if reply.first() == Some(&b'-') {
            let staged = push_scratch_bytes(ctx.scratch, &reply);
            ctx.shared.recycle_reply_buf(reply);
            return OwnerApplyReply::Immediate(staged);
        }
        let n = resp_int(&reply).expect("durable counted command returned integer RESP");
        ctx.shared.recycle_reply_buf(reply);
        if let Some(assignment) = assignment {
            ctx.shared.push_owner_durable_reply(OwnerDurableReply {
                origin: ctx.from,
                token: ctx.token,
                assignment,
                body: OwnerDurableReplyBody::Int(n),
            });
            OwnerApplyReply::Deferred
        } else {
            OwnerApplyReply::Immediate(StagedReply::Int(n))
        }
    } else if let Some(assignment) = assignment {
        ctx.shared.push_owner_durable_reply(OwnerDurableReply {
            origin: ctx.from,
            token: ctx.token,
            assignment,
            body: OwnerDurableReplyBody::Bytes(reply),
        });
        OwnerApplyReply::Deferred
    } else {
        let staged = push_scratch_bytes(ctx.scratch, &reply);
        ctx.shared.recycle_reply_buf(reply);
        OwnerApplyReply::Immediate(staged)
    }
}

fn execute_durable_everysec_apply_reply<O: PlaneObserver + 'static>(
    ctx: OwnerDurableApply<'_, '_, O>,
) -> StagedReply {
    let mut reply = ctx.shared.take_reply_buf();
    ctx.shared.execute_durable_everysec_into(
        ExecOrigin::Fabric(ctx.from),
        ctx.argv,
        ctx.proto,
        ctx.selected,
        ctx.namespace,
        &mut reply,
    );
    if ctx.counted {
        if reply.first() == Some(&b'-') {
            let staged = push_scratch_bytes(ctx.scratch, &reply);
            ctx.shared.recycle_reply_buf(reply);
            staged
        } else {
            let n = resp_int(&reply).expect("durable counted command returned integer RESP");
            ctx.shared.recycle_reply_buf(reply);
            StagedReply::Int(n)
        }
    } else {
        let staged = push_scratch_bytes(ctx.scratch, &reply);
        ctx.shared.recycle_reply_buf(reply);
        staged
    }
}

fn execute_checkpoint_apply_reply<O: PlaneObserver + 'static>(
    shared: &Shared<O>,
    from: CellId,
    token: FabricToken,
    argv: &[&[u8]],
    proto: Protocol,
    scratch: &mut Vec<u8>,
) -> OwnerApplyReply {
    let Some(meta) = lookup(argv[0]).filter(|meta| checkpoint_command(meta.id)) else {
        return OwnerApplyReply::Immediate(push_scratch_error(
            scratch,
            proto,
            "ERR unknown checkpoint command",
        ));
    };
    let request = match parse_checkpoint_request(meta.id, argv) {
        Ok(request) => request,
        Err(error) => return OwnerApplyReply::Immediate(push_scratch_error(scratch, proto, error)),
    };
    if request.target.is_some() {
        return OwnerApplyReply::Immediate(push_scratch_error(
            scratch,
            proto,
            "ERR INF.CKPT CELL target must be resolved by the origin cell",
        ));
    }
    let wait = request.mode == CheckpointReplyMode::InfCkptWait;
    match shared.try_queue_checkpoint_publish(wait) {
        Ok(Some(waiter)) if wait => {
            shared.push_owner_checkpoint_reply(OwnerCheckpointReply {
                origin: from,
                token,
                waiter,
            });
            OwnerApplyReply::Deferred
        }
        Ok(None) if !wait => {
            let text = match request.mode {
                CheckpointReplyMode::BgsaveAccepted => "Background saving started",
                CheckpointReplyMode::InfCkptAccepted => "OK",
                CheckpointReplyMode::InfCkptWait => unreachable!("wait handled above"),
            };
            OwnerApplyReply::Immediate(push_scratch_simple(scratch, proto, text))
        }
        Ok(_) => panic!("checkpoint waiter shape did not match owner apply request mode"),
        Err(error) => {
            OwnerApplyReply::Immediate(push_scratch_error(scratch, proto, error.message()))
        }
    }
}

impl<O: PlaneObserver + 'static> ServerPlane<O> {
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
                durability: RefCell::new(DurabilityCell::new()),
                durable_write_refusal: Cell::new(None),
                log_writer: RefCell::new(None),
                durability_gate: WatermarkGate::new(),
                durability_assignment_gate: KeyedGate::new(),
                durability_assignment_tickets: RefCell::new(VecDeque::new()),
                durability_assignment_next_ticket: Cell::new(1),
                ns_catalog_publish_gate: KeyedGate::new(),
                ns_catalog_publish_requests: RefCell::new(VecDeque::new()),
                ns_catalog_publish_next_ticket: Cell::new(1),
                ns_catalog_publish_enabled: Cell::new(false),
                checkpoint_publish_gate: KeyedGate::new(),
                checkpoint_publish_requests: RefCell::new(VecDeque::new()),
                checkpoint_publish_next_ticket: Cell::new(1),
                checkpoint_publish_enabled: Cell::new(false),
                checkpoint_publish_inflight: Cell::new(false),
                #[cfg(test)]
                durability_default_policy: Cell::new(None),
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
                pubsub: RefCell::new(PubSubCell::new(cells)),
                pub_queue: RefCell::new(VecDeque::new()),
                pub_pump_active: Cell::new(false),
                owner_durable_replies: RefCell::new(VecDeque::new()),
                owner_durable_pump_active: Cell::new(false),
                owner_checkpoint_replies: RefCell::new(VecDeque::new()),
                owner_checkpoint_pump_active: Cell::new(false),
                cob_pubsub: Cell::new((0, 0, 0)),
            }),
            listener,
            started: false,
            inbox: Vec::new(),
            reply_scratch: Vec::new(),
            staged_replies: Vec::new(),
            log_ops: Vec::new(),
            log_segment_maintenance: None,
            ns_catalog_ops: Vec::new(),
            ns_catalog_publisher: None,
            ns_catalog_active_ticket: None,
            checkpoint_ops: Vec::new(),
            checkpoint_publisher: None,
            checkpoint_active: None,
            checkpoint_next_id: Some(CheckpointId::FIRST_LIVE),
            park_flags: None,
            expiry_lag: 0,
            // MAX forces one push on the first MAINTAIN (boot-time config).
            config_pushed: u64::MAX,
            everysec_timer_armed: false,
        }
    }

    /// Wires this plane's slot of the doorbell-wakeup park board (the same
    /// `Arc` goes to every cell's fabric via `CellFabric::set_wakeups`).
    pub fn set_park_flags(&mut self, flags: Arc<Vec<AtomicBool>>) {
        self.park_flags = Some(flags);
    }

    /// Install the cell-local log writer.
    ///
    /// Boot code owns directory scans, file opens, and preallocation. The
    /// plane only receives an already-open active segment writer; this keeps
    /// LOG-step orchestration in `inf-server` without moving filesystem
    /// policy into `inf-log` or `inf-runtime`.
    pub fn install_log_writer(&self, writer: LogWriteIo) {
        assert!(self.shared.log_writer.borrow().is_none(), "log writer already installed");
        if let Some(publisher) = &self.ns_catalog_publisher {
            assert_ne!(
                writer.token(),
                publisher.token(),
                "log writer and namespace catalog publisher tokens collide"
            );
        }
        if let Some(maintenance) = &self.log_segment_maintenance {
            assert_ne!(
                writer.token(),
                maintenance.token(),
                "log writer and segment maintenance tokens collide"
            );
        }
        if let Some(publisher) = &self.checkpoint_publisher {
            assert_ne!(
                writer.token(),
                publisher.token(),
                "log writer and checkpoint publisher tokens collide"
            );
        }
        *self.shared.log_writer.borrow_mut() = Some(writer);
    }

    pub fn install_log_segment_maintenance(&mut self, maintenance: LogSegmentMaintenance) {
        assert!(
            self.log_segment_maintenance.is_none(),
            "log segment maintenance already installed"
        );
        if let Some(writer) = self.shared.log_writer.borrow().as_ref() {
            assert_ne!(
                writer.token(),
                maintenance.token(),
                "log writer and segment maintenance tokens collide"
            );
        }
        if let Some(publisher) = &self.ns_catalog_publisher {
            assert_ne!(
                publisher.token(),
                maintenance.token(),
                "namespace catalog publisher and segment maintenance tokens collide"
            );
        }
        if let Some(publisher) = &self.checkpoint_publisher {
            assert_ne!(
                publisher.token(),
                maintenance.token(),
                "checkpoint publisher and segment maintenance tokens collide"
            );
        }
        self.log_segment_maintenance = Some(maintenance);
    }

    /// Install the live namespace-catalog publisher used by `INF.NS`
    /// CREATE/DROP when production boot has a data root. The publisher owns
    /// only state-machine state; all syscalls still flow through
    /// `BackendDriver` via `LoopCx` ops.
    pub fn install_namespace_catalog_publisher(
        &mut self,
        publisher: NamespaceCatalogLivePublisher,
    ) {
        assert!(
            self.ns_catalog_publisher.is_none(),
            "namespace catalog publisher already installed"
        );
        if let Some(writer) = self.shared.log_writer.borrow().as_ref() {
            assert_ne!(
                writer.token(),
                publisher.token(),
                "namespace catalog publisher and log writer tokens collide"
            );
        }
        if let Some(maintenance) = &self.log_segment_maintenance {
            assert_ne!(
                publisher.token(),
                maintenance.token(),
                "namespace catalog publisher and segment maintenance tokens collide"
            );
        }
        if let Some(checkpoint) = &self.checkpoint_publisher {
            assert_ne!(
                publisher.token(),
                checkpoint.token(),
                "namespace catalog publisher and checkpoint publisher tokens collide"
            );
        }
        self.shared.ns_catalog_publish_enabled.set(true);
        self.ns_catalog_publisher = Some(publisher);
    }

    /// Install the live checkpoint publisher used by `INF.CKPT`/`BGSAVE`.
    /// The publisher owns only state-machine state over an already-open
    /// checkpoint directory; all file effects still flow through `LoopCx`.
    pub fn install_checkpoint_publisher(&mut self, publisher: LiveCheckpointPublisher) {
        assert!(self.checkpoint_publisher.is_none(), "checkpoint publisher already installed");
        if let Some(writer) = self.shared.log_writer.borrow().as_ref() {
            assert_ne!(
                writer.token(),
                publisher.token(),
                "checkpoint publisher and log writer tokens collide"
            );
        }
        if let Some(maintenance) = &self.log_segment_maintenance {
            assert_ne!(
                publisher.token(),
                maintenance.token(),
                "checkpoint publisher and segment maintenance tokens collide"
            );
        }
        if let Some(ns_catalog) = &self.ns_catalog_publisher {
            assert_ne!(
                publisher.token(),
                ns_catalog.token(),
                "checkpoint publisher and namespace catalog publisher tokens collide"
            );
        }
        self.shared.checkpoint_publish_enabled.set(true);
        self.checkpoint_publisher = Some(publisher);
    }

    /// Seed the live checkpoint id allocator after loading a durable recovery
    /// MANIFEST. The plane owns the allocator because checkpoint-begin records,
    /// checkpoint image names, and the live publisher are all cell-local.
    pub fn seed_checkpoint_next_id_after(&mut self, checkpoint_id: CheckpointId) {
        assert!(self.checkpoint_active.is_none(), "checkpoint publish already active");
        assert!(
            self.shared.checkpoint_publish_requests.borrow().is_empty(),
            "checkpoint publish queue must be empty before seeding"
        );
        self.checkpoint_next_id = checkpoint_id.next();
    }

    /// Live connections (tests, stats).
    pub fn connections(&self) -> usize {
        self.shared.conns.borrow().live
    }

    /// Outstanding async work: pending fabric replies, credit waiters, and
    /// namespace catalog DDL publish waiters. Quiescence (sim) means zero.
    pub fn suspended(&self) -> usize {
        self.shared.gate.pending()
            + self.shared.credit_waiters.waiting()
            + self.shared.ns_catalog_publish_gate.pending()
            + self.shared.checkpoint_publish_gate.pending()
            + self.shared.owner_checkpoint_replies.borrow().len()
    }

    /// Memory attribution for this cell's keyspace slice (sim accounting
    /// oracle, tooling — never the data plane).
    pub fn keyspace_report(&self) -> inf_store::MemoryReport {
        self.shared.store.borrow().report()
    }

    /// Pub/sub registry gauges `(owned channels, patterns, state bytes)` —
    /// the sim teardown oracle asserts all three return to zero once every
    /// subscriber unwound (M1-S15).
    pub fn pubsub_gauges(&self) -> (u64, u64, usize) {
        let ps = self.shared.pubsub.borrow();
        (ps.live_owned_channel_count(), ps.live_pattern_count(), ps.state_bytes())
    }

    /// Cell-local durability watermark, encoded in log order.
    pub fn durability_watermark(&self) -> u64 {
        self.shared.durability_gate.watermark()
    }

    #[cfg(test)]
    fn enable_default_always_for_test(&self) {
        self.shared.durability_default_policy.set(Some(NsFsyncPolicy::Always));
    }

    #[cfg(test)]
    fn enable_default_everysec_for_test(&self) {
        self.shared.durability_default_policy.set(Some(NsFsyncPolicy::Everysec));
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
        if self.shared.route_local_only {
            return false;
        }
        let sub = (argv.len() > 1).then(|| argv.arg(1));
        if self.shared.cells > 1 && is_scatter(meta.id, sub) {
            return true;
        }
        extract_keys(meta, argv).any(|key| !self.shared.router.is_local(key, self.shared.cell))
    }

    fn needs_durable_pump(&self, argv: &ArgvRef<'_>, namespace: ConnNamespace) -> bool {
        let Some(meta) = lookup(argv.arg(0)) else { return false };
        if !arity_ok(meta, argv.len()) {
            return false;
        }
        self.shared.durable_namespace_policy(meta, namespace).is_some()
    }

    fn needs_namespace_catalog_pump(&self, argv: &ArgvRef<'_>) -> bool {
        if !self.shared.ns_catalog_publish_enabled.get() {
            return false;
        }
        let Some(meta) = lookup(argv.arg(0)) else { return false };
        if !arity_ok(meta, argv.len()) {
            return false;
        }
        namespace_catalog_ddl(meta.id, (argv.len() > 1).then(|| argv.arg(1)))
    }

    fn needs_checkpoint_pump(&self, argv: &ArgvRef<'_>) -> bool {
        let Some(meta) = lookup(argv.arg(0)) else { return false };
        if !arity_ok(meta, argv.len()) {
            return false;
        }
        checkpoint_command(meta.id)
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

    fn is_log_writer_token(&self, token: CompletionToken) -> bool {
        self.shared.log_writer.borrow().as_ref().is_some_and(|writer| writer.token() == token)
    }

    fn is_namespace_catalog_token(&self, token: CompletionToken) -> bool {
        self.ns_catalog_publisher.as_ref().is_some_and(|publisher| publisher.token() == token)
    }

    fn is_checkpoint_token(&self, token: CompletionToken) -> bool {
        self.checkpoint_publisher.as_ref().is_some_and(|publisher| publisher.token() == token)
    }

    fn is_log_segment_maintenance_token(&self, token: CompletionToken) -> bool {
        self.log_segment_maintenance
            .as_ref()
            .is_some_and(|maintenance| maintenance.token() == token)
    }

    fn complete_namespace_catalog_publish(&mut self, cx: &mut LoopCx<'_>, completion: Completion) {
        let publisher = self
            .ns_catalog_publisher
            .as_mut()
            .expect("namespace catalog token implies installed publisher");
        let event = publisher
            .on_completion(cx.pool, completion)
            .unwrap_or_else(|error| Self::fatal_namespace_catalog_publish_error(error));
        if event == NamespaceCatalogLivePublishEvent::Completed {
            let ticket = self
                .ns_catalog_active_ticket
                .take()
                .expect("completed namespace catalog publish without an active ticket");
            self.shared.complete_namespace_catalog_publish(ticket);
        }
    }

    fn progress_namespace_catalog_publish(&mut self, cx: &mut LoopCx<'_>) {
        let Some(publisher) = self.ns_catalog_publisher.as_mut() else {
            return;
        };
        if publisher.is_idle() {
            let next = self.shared.ns_catalog_publish_requests.borrow_mut().pop_front();
            if let Some(request) = next {
                assert!(
                    self.ns_catalog_active_ticket.is_none(),
                    "idle namespace catalog publisher still had an active ticket"
                );
                publisher
                    .start(request.snapshot)
                    .unwrap_or_else(|error| Self::fatal_namespace_catalog_publish_error(error));
                self.ns_catalog_active_ticket = Some(request.ticket);
            }
        }
        self.ns_catalog_ops.clear();
        publisher
            .drive(cx.pool, &mut self.ns_catalog_ops)
            .unwrap_or_else(|error| Self::fatal_namespace_catalog_publish_error(error));
        for op in self.ns_catalog_ops.drain(..) {
            cx.push(op);
        }
    }

    fn fatal_namespace_catalog_publish_error(error: NamespaceCatalogPublishError) -> ! {
        panic!("fatal namespace catalog publish error: {error}");
    }

    fn complete_checkpoint_publish(&mut self, cx: &mut LoopCx<'_>, completion: Completion) {
        let publisher = self
            .checkpoint_publisher
            .as_mut()
            .expect("checkpoint token implies installed publisher");
        let event = publisher
            .on_completion(cx.pool, completion)
            .unwrap_or_else(|error| Self::fatal_checkpoint_publish_error(error));
        if event == LiveCheckpointPublishEvent::Completed {
            let Some(ActiveCheckpointPublish::Publishing { ticket, checkpoint }) =
                self.checkpoint_active.take()
            else {
                panic!("completed checkpoint publish without active publish state");
            };
            let _ = checkpoint;
            let now_ms = wall_ms(&self.shared.node, cx.now);
            self.shared.complete_checkpoint_publish(ticket, now_ms);
        }
    }

    fn progress_checkpoint_publish(&mut self, cx: &mut LoopCx<'_>) {
        if self.checkpoint_publisher.is_none() {
            return;
        }
        if self.checkpoint_active.is_none() {
            let next = self.shared.checkpoint_publish_requests.borrow_mut().pop_front();
            if let Some(request) = next {
                if self.shared.durability.borrow().pending_frame_len_bytes().is_some() {
                    self.shared.checkpoint_publish_requests.borrow_mut().push_front(request);
                    return;
                }
                let Some(checkpoint_id) = self.checkpoint_next_id else {
                    panic!("checkpoint id exhausted");
                };
                self.checkpoint_next_id = checkpoint_id.next();
                let parts = {
                    let store = self.shared.store.borrow();
                    encode_checkpoint_keyspace_snapshot_parts(
                        &store,
                        CheckpointKeyspaceSnapshotConfig::new(cx.now),
                    )
                    .unwrap_or_else(|error| Self::fatal_checkpoint_snapshot_error(error))
                };
                self.shared
                    .durability
                    .borrow_mut()
                    .stage_checkpoint_begin(checkpoint_id)
                    .unwrap_or_else(|error| panic!("fatal checkpoint-begin stage error: {error}"));
                self.checkpoint_active = Some(ActiveCheckpointPublish::AwaitLog {
                    ticket: request.ticket,
                    checkpoint_id,
                    parts,
                });
            }
        }

        if matches!(self.checkpoint_active, Some(ActiveCheckpointPublish::AwaitLog { .. })) {
            self.seal_log(cx);
        }

        let ready = match self.checkpoint_active.as_ref() {
            Some(ActiveCheckpointPublish::AwaitDurable { watermark_key, .. }) => {
                self.shared.durability_gate.watermark() >= *watermark_key
            }
            _ => false,
        };
        if ready {
            let Some(ActiveCheckpointPublish::AwaitDurable { ticket, checkpoint, parts, .. }) =
                self.checkpoint_active.take()
            else {
                unreachable!("ready checkpoint state checked above");
            };
            let image = encode_checkpoint_keyspace_snapshot_image_from_parts(
                self.shared.cell,
                checkpoint,
                &parts,
            )
            .unwrap_or_else(|error| Self::fatal_checkpoint_snapshot_error(error));
            let manifest =
                RecoveryManifest::new(checkpoint, self.live_checkpoint_segment_catalog(checkpoint))
                    .unwrap_or_else(|error| {
                        panic!("fatal checkpoint manifest construction error: {error}")
                    });
            let publisher = self
                .checkpoint_publisher
                .as_mut()
                .expect("checkpoint active state requires installed publisher");
            publisher
                .start(checkpoint, image, &manifest)
                .unwrap_or_else(|error| Self::fatal_checkpoint_publish_error(error));
            self.checkpoint_active =
                Some(ActiveCheckpointPublish::Publishing { ticket, checkpoint });
        }

        let Some(publisher) = self.checkpoint_publisher.as_mut() else {
            return;
        };
        self.checkpoint_ops.clear();
        publisher
            .drive(cx.pool, &mut self.checkpoint_ops)
            .unwrap_or_else(|error| Self::fatal_checkpoint_publish_error(error));
        for op in self.checkpoint_ops.drain(..) {
            cx.push(op);
        }
    }

    fn live_checkpoint_segment_catalog(
        &self,
        checkpoint: CheckpointRef,
    ) -> inf_log::SegmentCatalog {
        let active = self
            .shared
            .log_writer
            .borrow()
            .as_ref()
            .expect("checkpoint publish requires installed log writer")
            .active_segment();
        let begin = SegmentId::new(checkpoint.begin_lsn().segment())
            .expect("checkpoint begin LSN segment is valid");
        let mut names = Vec::new();
        let mut raw = begin.get();
        loop {
            let segment = SegmentId::new(raw).expect("active segment range is valid");
            names.push(segment.file_name());
            if raw == active.get() {
                break;
            }
            raw = raw.checked_add(1).expect("segment id range overflowed");
        }
        scan_segment_names(names.iter().map(String::as_str))
            .expect("constructed checkpoint segment catalog is contiguous")
    }

    fn fatal_checkpoint_snapshot_error(error: CheckpointKeyspacePublishError) -> ! {
        panic!("fatal checkpoint snapshot error: {error}");
    }

    fn fatal_checkpoint_publish_error(error: LiveCheckpointPublishError) -> ! {
        panic!("fatal checkpoint publish error: {error}");
    }

    fn complete_log_segment_maintenance(&mut self, cx: &mut LoopCx<'_>, completion: Completion) {
        let maintenance = self
            .log_segment_maintenance
            .as_mut()
            .expect("segment maintenance token implies installed maintenance");
        self.log_ops.clear();
        let event = {
            let mut writer = self.shared.log_writer.borrow_mut();
            let writer =
                writer.as_mut().expect("segment maintenance token implies installed log writer");
            maintenance
                .on_completion(writer, completion, &mut self.log_ops)
                .unwrap_or_else(|error| Self::fatal_log_segment_maintenance_error(error))
        };
        match event {
            LogSegmentMaintenanceEvent::Progress => {}
            LogSegmentMaintenanceEvent::Prepared { .. } => {}
            LogSegmentMaintenanceEvent::PreallocateFailed { segment, fd, errno } => {
                if errno == ENOSPC_ERRNO {
                    self.shared
                        .durable_write_refusal
                        .set(Some(DurableWriteRefusal::LogPreallocateEnospc { segment, errno }));
                } else {
                    Self::fatal_log_segment_maintenance_error(
                        LogSegmentMaintenanceError::Preallocate { segment, fd, errno },
                    );
                }
            }
        }
        for op in self.log_ops.drain(..) {
            cx.push(op);
        }
    }

    fn progress_log_segment_maintenance(&mut self, cx: &mut LoopCx<'_>) {
        if self.shared.durable_write_refusal.get().is_some() {
            return;
        }
        let Some(maintenance) = self.log_segment_maintenance.as_mut() else {
            return;
        };
        self.log_ops.clear();
        {
            let writer = self.shared.log_writer.borrow();
            let Some(writer) = writer.as_ref() else {
                return;
            };
            maintenance
                .drive(writer, &mut self.log_ops)
                .unwrap_or_else(|error| Self::fatal_log_segment_maintenance_error(error));
        }
        for op in self.log_ops.drain(..) {
            cx.push(op);
        }
    }

    fn fatal_log_segment_maintenance_error(error: LogSegmentMaintenanceError) -> ! {
        panic!("fatal log segment maintenance error: {error}");
    }

    fn complete_log_write(&mut self, cx: &mut LoopCx<'_>, completion: Completion) {
        let mut writer = self.shared.log_writer.borrow_mut();
        let writer = writer.as_mut().expect("log writer token implies installed writer");
        self.log_ops.clear();
        let completed = writer
            .on_completion(cx.pool, &mut self.log_ops, completion)
            .unwrap_or_else(|error| panic!("fatal log write completion error: {error}"));
        match completed {
            LogWriteCompletion::FrameWritten(_) => {}
            LogWriteCompletion::SyncQueued { .. } => {}
            LogWriteCompletion::SealProgress { .. } => {}
            LogWriteCompletion::SealFinalized { .. } => {}
            LogWriteCompletion::FrameDurable(meta) => {
                self.shared.advance_durability_watermark(meta);
            }
        }
        for op in self.log_ops.drain(..) {
            cx.push(op);
        }
    }

    fn queue_everysec_sync(&mut self, cx: &mut LoopCx<'_>) {
        let mut writer = self.shared.log_writer.borrow_mut();
        let Some(writer) = writer.as_mut() else {
            return;
        };
        self.log_ops.clear();
        let _queued = writer
            .queue_pending_sync(&mut self.log_ops, FileSyncMode::DataOnly)
            .unwrap_or_else(|error| panic!("fatal everysec sync queue error: {error}"));
        for op in self.log_ops.drain(..) {
            cx.push(op);
        }
    }

    fn arm_everysec_timer(&mut self, cx: &mut LoopCx<'_>) {
        if self.everysec_timer_armed {
            return;
        }
        cx.timers.insert(cx.now.saturating_add(LOG_EVERYSEC_INTERVAL), LOG_EVERYSEC_TIMER_KEY);
        self.everysec_timer_armed = true;
    }

    fn flush_persistence_stats(&self) {
        let node = &self.shared.node;
        let pending = self.shared.durability.borrow().log_staging_bytes();
        node.pending_log_bytes.set(pending);
        let durable = self.shared.durability_gate.watermark();
        node.last_durable_lsn.set(durable);

        let writer = self.shared.log_writer.borrow();
        let Some(writer) = writer.as_ref() else {
            node.log_writer_installed.set(0);
            node.log_active_segment.set(0);
            node.log_active_offset_bytes.set(0);
            node.log_pending_unsynced.set(0);
            node.watermark_lag_lsn.set(0);
            return;
        };

        let active = log_watermark_key(Lsn::new(
            writer.active_segment().get(),
            writer.active_offset_bytes(),
        ));
        node.log_writer_installed.set(1);
        node.log_active_segment.set(u64::from(writer.active_segment().get()));
        node.log_active_offset_bytes.set(u64::from(writer.active_offset_bytes()));
        node.log_pending_unsynced.set(if writer.has_pending_unsynced() { 1 } else { 0 });
        node.watermark_lag_lsn.set(active.saturating_sub(durable));
    }

    fn handle_seal_error(error: LogWriteIoError) {
        match error {
            LogWriteIoError::WriteAlreadyInFlight { .. }
            | LogWriteIoError::WriteBufferUnavailable => {}
            other => panic!("fatal log seal error: {other}"),
        }
    }
}

fn log_watermark_key(lsn: Lsn) -> u64 {
    (u64::from(lsn.segment()) << 32) | u64::from(lsn.offset())
}

impl<O: PlaneObserver + 'static> CellPlane for ServerPlane<O> {
    fn on_completion(&mut self, cx: &mut LoopCx<'_>, c: Completion) {
        if self.is_log_writer_token(c.token) {
            self.complete_log_write(cx, c);
            return;
        }
        if self.is_namespace_catalog_token(c.token) {
            self.complete_namespace_catalog_publish(cx, c);
            return;
        }
        if self.is_checkpoint_token(c.token) {
            self.complete_checkpoint_publish(cx, c);
            return;
        }
        if self.is_log_segment_maintenance_token(c.token) {
            self.complete_log_segment_maintenance(cx, c);
            return;
        }

        match c.result {
            CompletionResult::Accepted { fd } => {
                let key = self.shared.conns.borrow_mut().insert(Conn {
                    fd,
                    parser: ConnParser::new(ParserLimits::default()),
                    cx: ConnCx {
                        proto: Protocol::Resp2,
                        id: 0,
                        namespace: ConnNamespace::default(),
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
            CompletionResult::FileOpened { .. }
            | CompletionResult::FileRead { .. }
            | CompletionResult::FileWritten { .. }
            | CompletionResult::FileDone
            | CompletionResult::FileClosed => {
                panic!("unexpected durability file completion in ServerPlane");
            }
            CompletionResult::Error { buf, .. } => {
                if c.token.class() == TokenClass::File {
                    if let Some(buf) = buf {
                        cx.pool.release(buf);
                    }
                    panic!("unexpected durability file error in ServerPlane");
                }
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

    fn on_timer(&mut self, cx: &mut LoopCx<'_>, key: u64) {
        if key == LOG_EVERYSEC_TIMER_KEY {
            self.everysec_timer_armed = false;
            self.queue_everysec_sync(cx);
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
        let mut pubs: Vec<OwnerPub> = Vec::new();
        let mut durable_queued = false;
        let mut checkpoint_queued = false;
        let now = cx.now;
        let drained = shared.fabric.borrow_mut().drain(FABRIC_DRAIN_MAX, |from, op| {
            handle_fabric_op(
                shared,
                now,
                from,
                op,
                scratch,
                staged,
                &mut pubs,
                &mut durable_queued,
                &mut checkpoint_queued,
                &mut orphans,
            );
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
        drop(fabric);
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
        if durable_queued && !self.shared.owner_durable_pump_active.get() {
            self.shared.owner_durable_pump_active.set(true);
            let shared = Rc::clone(&self.shared);
            let _ = cx.executor.poll_immediate(owner_durable_reply_pump(shared));
        }
        if checkpoint_queued && !self.shared.owner_checkpoint_pump_active.get() {
            self.shared.owner_checkpoint_pump_active.set(true);
            let shared = Rc::clone(&self.shared);
            let _ = cx.executor.poll_immediate(owner_checkpoint_reply_pump(shared));
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
                let mut iter = parser.feed(data);
                while let Some(parsed) = iter.next() {
                    match parsed {
                        Parsed::Command(argv) | Parsed::Inline(argv) => {
                            commands += 1;
                            let defer = pump_was_active
                                || spawn_first.is_some()
                                || !deferred.is_empty()
                                || self.needs_fabric(&argv)
                                || self.needs_namespace_catalog_pump(&argv)
                                || self.needs_checkpoint_pump(&argv)
                                || self.needs_durable_pump(&argv, conn_cx.namespace);
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
                            let mut w = RespWriter::new(out, conn_cx.proto);
                            w.error(&format!("ERR Protocol error: {e:?}"));
                            protocol_error = true;
                            break;
                        }
                    }
                }
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
    }

    fn maintain(&mut self, cx: &mut LoopCx<'_>) {
        self.shared.now.set(cx.now);
        self.progress_namespace_catalog_publish(cx);
        self.progress_checkpoint_publish(cx);
        self.progress_log_segment_maintenance(cx);
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
        // ---- stats flush
        let node = &self.shared.node;
        node.recv_dropped.set(self.shared.recv_dropped.get());
        node.fabric_rtt_p50_ns.set(self.shared.rtt_ns.borrow().percentile(50.0));
        {
            let durability = self.shared.durability.borrow();
            node.log_staging_bytes.set(durability.log_staging_bytes());
            node.log_staging_capacity_bytes.set(durability.log_staging_capacity_bytes());
        }
        self.flush_persistence_stats();
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
    }

    fn seal_log(&mut self, cx: &mut LoopCx<'_>) {
        if self.shared.durability.borrow().pending_frame_len_bytes().is_none() {
            return;
        }
        let checkpoint_sync_required =
            matches!(self.checkpoint_active, Some(ActiveCheckpointPublish::AwaitLog { .. }));
        let sync_required = checkpoint_sync_required
            || !self.shared.durability_assignment_tickets.borrow().is_empty();

        self.log_ops.clear();
        let mut arm_everysec = false;
        {
            let mut writer = self.shared.log_writer.borrow_mut();
            let Some(writer) = writer.as_mut() else {
                panic!("log staging is non-empty but no log writer is installed");
            };
            if writer.in_flight() {
                return;
            }
            let mut durability = self.shared.durability.borrow_mut();
            let queued = if sync_required {
                writer.queue_frame_synced(
                    &mut durability,
                    cx.pool,
                    &mut self.log_ops,
                    FileSyncMode::DataOnly,
                )
            } else {
                writer.queue_frame(&mut durability, cx.pool, &mut self.log_ops)
            };
            match queued {
                Ok(Some(meta)) => {
                    if sync_required {
                        self.shared.complete_durability_assignments(meta);
                    } else {
                        arm_everysec = true;
                    }
                    if matches!(
                        self.checkpoint_active,
                        Some(ActiveCheckpointPublish::AwaitLog { .. })
                    ) {
                        let Some(ActiveCheckpointPublish::AwaitLog {
                            ticket,
                            checkpoint_id,
                            parts,
                        }) = self.checkpoint_active.take()
                        else {
                            unreachable!("checkpoint state matched AwaitLog");
                        };
                        let checkpoint = CheckpointRef::new(checkpoint_id, meta.frame_start());
                        let watermark_key = log_watermark_key(meta.frame_end());
                        self.checkpoint_active = Some(ActiveCheckpointPublish::AwaitDurable {
                            ticket,
                            checkpoint,
                            parts,
                            watermark_key,
                        });
                    }
                }
                Ok(None) => {}
                Err(error) => Self::handle_seal_error(error),
            }
        }
        if arm_everysec {
            self.arm_everysec_timer(cx);
        }
        for op in self.log_ops.drain(..) {
            cx.push(op);
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
fn handle_fabric_op<O: PlaneObserver + 'static>(
    shared: &Shared<O>,
    now: Nanos,
    from: CellId,
    op: Op<'_>,
    scratch: &mut Vec<u8>,
    staged: &mut Vec<(CellId, FabricToken, StagedReply)>,
    pubs: &mut Vec<OwnerPub>,
    durable_queued: &mut bool,
    checkpoint_queued: &mut bool,
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
        Op::Apply { token, namespace, cmd, args, .. } => {
            let argv = args.as_slice();
            // Internal pub/sub fabric vocabulary (M1-S10) — intercepted
            // ahead of `execute`, so it needs no registry entries and stays
            // invisible to clients (an `INF.PUBFAN` typed by a client is an
            // unknown command). One first-byte gate keys the comparisons.
            if argv[0].first().is_some_and(|b| b | 0x20 == b'i')
                && handle_pubsub_apply(shared, from, token, argv, scratch, staged, pubs)
            {
                return;
            }
            // Apply v1 carries the selected namespace explicitly. `cmd`
            // keeps only the RESP protocol byte.
            let proto = if cmd & 0x0F == 3 { Protocol::Resp3 } else { Protocol::Resp2 };
            let selected = ConnNamespace::from_id(namespace);
            if lookup(argv[0]).is_some_and(|meta| checkpoint_command(meta.id)) {
                match execute_checkpoint_apply_reply(shared, from, token, argv, proto, scratch) {
                    OwnerApplyReply::Immediate(reply) => staged.push((from, token, reply)),
                    OwnerApplyReply::Deferred => *checkpoint_queued = true,
                }
                return;
            }
            // Single-key DEL/UNLINK/EXISTS/TOUCH contributions and DBSIZE
            // stay typed for origin-side aggregation; everything else
            // returns the raw RESP reply.
            let counted = argv.len() == 2
                && [&b"DEL"[..], b"UNLINK", b"EXISTS", b"TOUCH"]
                    .iter()
                    .any(|n| argv[0].eq_ignore_ascii_case(n));
            let durable = lookup(argv[0])
                .filter(|meta| arity_ok(meta, argv.len()))
                .and_then(|meta| shared.durable_namespace_policy(meta, selected));
            if let Some(durable) = durable {
                let request = OwnerDurableApply {
                    shared,
                    from,
                    token,
                    argv,
                    proto,
                    selected,
                    namespace: durable.id,
                    counted,
                    scratch,
                };
                match durable.fsync {
                    NsFsyncPolicy::Always => match execute_durable_apply_reply(request) {
                        OwnerApplyReply::Immediate(reply) => staged.push((from, token, reply)),
                        OwnerApplyReply::Deferred => *durable_queued = true,
                    },
                    NsFsyncPolicy::Everysec => {
                        staged.push((from, token, execute_durable_everysec_apply_reply(request)));
                    }
                }
            } else if counted {
                let n = shared.apply_counted(ExecOrigin::Fabric(from), argv[0], argv[1], selected);
                staged.push((from, token, StagedReply::Int(n)));
            } else if argv.len() == 1 && argv[0].eq_ignore_ascii_case(b"DBSIZE") {
                let n = shared.apply_dbsize(ExecOrigin::Fabric(from), selected);
                staged.push((from, token, StagedReply::Int(n)));
            } else {
                let start = scratch.len();
                shared.execute_owned_into(
                    ExecOrigin::Fabric(from),
                    argv,
                    proto,
                    0,
                    selected,
                    scratch,
                );
                staged.push((from, token, StagedReply::Bytes(start, scratch.len())));
            }
        }
        Op::Read { token, key, .. } => {
            let start = scratch.len();
            // M0 vocabulary: the typed Read has no namespace field; it
            // serves db0 (M1+ paths ship GETs as Apply).
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
        Op::Batch { ops } => {
            for nested in ops {
                handle_fabric_op(
                    shared,
                    now,
                    from,
                    nested,
                    scratch,
                    staged,
                    pubs,
                    durable_queued,
                    checkpoint_queued,
                    orphans,
                );
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
    /// Local `always` durable write: LOG assigns the frame-end key, then
    /// fsync completion advances the durability watermark that releases it.
    DurableAlways { assignment: GateWait<u64, u64>, reply: Vec<u8> },
    /// One remote `Apply` in flight; the owner's raw RESP reply parks in
    /// the gate if it lands before its turn.
    Remote { waiter: GateWait<u64, OwnedOutcome>, proto: Protocol },
    /// Split DEL/UNLINK/EXISTS/TOUCH (and scattered DBSIZE): locally-counted
    /// contributions in `acc`, remote per-key contributions in flight.
    Counted { waiters: Vec<GateWait<u64, OwnedOutcome>>, acc: i64, proto: Protocol },
    /// Split MGET: per-key replies reassemble into one array in argv order.
    Gather { parts: Vec<GatherPart>, proto: Protocol },
    /// Namespace DDL has applied locally; the visible ack waits for `META`.
    NamespaceCatalog { waiter: GateWait<u64, ()>, proto: Protocol },
    /// `INF.CKPT WAIT`: reply only after the checkpoint MANIFEST is durable.
    Checkpoint { waiter: GateWait<u64, ()>, proto: Protocol },
    /// Fanned MSET / scattered FLUSH: all legs must come back `+OK` (the
    /// first error leg wins the reply otherwise).
    AllOk { waiters: Vec<GateWait<u64, OwnedOutcome>>, proto: Protocol },
    /// Scattered namespace DDL: all cells must accept, then the origin
    /// publishes `META` before emitting `+OK`.
    AllOkThenNamespaceCatalog { waiters: Vec<GateWait<u64, OwnedOutcome>>, proto: Protocol },
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
/// every later reply serializes under; SELECT/INF.NS USE switch the namespace
/// every later command routes to).
fn is_conn_state(owned: &OwnedCmd) -> bool {
    lookup(owned.arg(0)).is_some_and(|m| {
        matches!(m.id, CommandId::Hello | CommandId::Select)
            || (m.id == CommandId::InfNs
                && owned.argc() > 1
                && owned.arg(1).eq_ignore_ascii_case(b"USE"))
            || checkpoint_command(m.id)
    })
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
            PendingReply::DurableAlways { assignment, reply } => {
                let watermark_key = assignment.await;
                shared.durability_gate.waiter(watermark_key).await;
                reply
            }
            PendingReply::Remote { waiter, proto } => {
                let outcome = waiter.await;
                inflight -= 1;
                render_outcome(&shared, outcome, proto)
            }
            PendingReply::Counted { waiters, mut acc, proto } => {
                let mut failure: Option<Vec<u8>> = None;
                for waiter in waiters {
                    match waiter.await {
                        OwnedOutcome::Int(n) => acc += n,
                        OwnedOutcome::Bytes(bytes)
                            if bytes.first() == Some(&b'-') && failure.is_none() =>
                        {
                            failure = Some(bytes)
                        }
                        other => debug_assert!(false, "counted apply returned {other:?}"),
                    }
                    inflight -= 1;
                }
                match failure {
                    Some(error) => error,
                    None => {
                        let mut reply = shared.take_reply_buf();
                        RespWriter::new(&mut reply, proto).int(acc);
                        reply
                    }
                }
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
            PendingReply::NamespaceCatalog { waiter, proto } => {
                waiter.await;
                let mut reply = shared.take_reply_buf();
                RespWriter::new(&mut reply, proto).simple("OK");
                reply
            }
            PendingReply::Checkpoint { waiter, proto } => {
                waiter.await;
                let mut reply = shared.take_reply_buf();
                RespWriter::new(&mut reply, proto).simple("OK");
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
            PendingReply::AllOkThenNamespaceCatalog { waiters, proto } => {
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
                if let Some(error) = failure {
                    panic!(
                        "fatal namespace DDL scatter failure after local mutation: {}",
                        String::from_utf8_lossy(&error)
                    );
                }
                let waiter = shared.queue_namespace_catalog_publish();
                waiter.await;
                let mut reply = shared.take_reply_buf();
                RespWriter::new(&mut reply, proto).simple("OK");
                reply
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
        CommandId::InfNs => namespace_catalog_ddl(id, sub),
        _ => false,
    }
}

fn namespace_catalog_ddl(id: CommandId, sub: Option<&[u8]>) -> bool {
    id == CommandId::InfNs
        && sub.is_some_and(|s| s.eq_ignore_ascii_case(b"CREATE") || s.eq_ignore_ascii_case(b"DROP"))
}

fn checkpoint_command(id: CommandId) -> bool {
    matches!(id, CommandId::Bgsave | CommandId::InfCkpt)
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum CheckpointReplyMode {
    InfCkptAccepted,
    InfCkptWait,
    BgsaveAccepted,
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
struct CheckpointRequest {
    mode: CheckpointReplyMode,
    target: Option<CellId>,
}

fn parse_cell_id_arg(arg: &[u8]) -> Option<CellId> {
    if arg.is_empty() {
        return None;
    }
    let mut raw: u32 = 0;
    for &byte in arg {
        if !byte.is_ascii_digit() {
            return None;
        }
        raw = raw.checked_mul(10)?.checked_add(u32::from(byte - b'0'))?;
        if raw > u32::from(u16::MAX) {
            return None;
        }
    }
    Some(CellId(raw as u16))
}

fn parse_checkpoint_request(
    id: CommandId,
    argv: &[&[u8]],
) -> Result<CheckpointRequest, &'static str> {
    match id {
        CommandId::Bgsave => {
            if argv.len() == 1 {
                Ok(CheckpointRequest { mode: CheckpointReplyMode::BgsaveAccepted, target: None })
            } else {
                Err("ERR syntax error")
            }
        }
        CommandId::InfCkpt => {
            let mut wait = false;
            let mut target = None;
            let mut i = 1;
            while i < argv.len() {
                let arg = argv[i];
                if arg.eq_ignore_ascii_case(b"WAIT") {
                    wait = true;
                    i += 1;
                } else if arg.eq_ignore_ascii_case(b"CELL") {
                    if i + 1 >= argv.len() || target.is_some() {
                        return Err("ERR syntax error");
                    }
                    let Some(cell) = parse_cell_id_arg(argv[i + 1]) else {
                        return Err("ERR syntax error");
                    };
                    target = Some(cell);
                    i += 2;
                } else {
                    return Err("ERR syntax error");
                }
            }
            let mode = if wait {
                CheckpointReplyMode::InfCkptWait
            } else {
                CheckpointReplyMode::InfCkptAccepted
            };
            Ok(CheckpointRequest { mode, target })
        }
        _ => unreachable!("checkpoint_command checked command id"),
    }
}

fn push_local_checkpoint_reply<O: PlaneObserver + 'static>(
    shared: &Rc<Shared<O>>,
    mode: CheckpointReplyMode,
    proto: Protocol,
    pending: &mut VecDeque<PendingReply>,
) {
    let wait = mode == CheckpointReplyMode::InfCkptWait;
    match shared.try_queue_checkpoint_publish(wait) {
        Ok(Some(waiter)) if wait => {
            pending.push_back(PendingReply::Checkpoint { waiter, proto });
        }
        Ok(None) if !wait => {
            let mut reply = shared.take_reply_buf();
            let mut writer = RespWriter::new(&mut reply, proto);
            match mode {
                CheckpointReplyMode::BgsaveAccepted => {
                    writer.simple("Background saving started");
                }
                CheckpointReplyMode::InfCkptAccepted => writer.simple("OK"),
                CheckpointReplyMode::InfCkptWait => unreachable!("wait handled above"),
            }
            pending.push_back(PendingReply::Done(reply));
        }
        Ok(_) => panic!("checkpoint waiter shape did not match request mode"),
        Err(error) => {
            let mut reply = shared.take_reply_buf();
            RespWriter::new(&mut reply, proto).error(error.message());
            pending.push_back(PendingReply::Done(reply));
        }
    }
}

async fn dispatch_checkpoint_command<O: PlaneObserver + 'static>(
    shared: &Rc<Shared<O>>,
    id: CommandId,
    argv: &[&[u8]],
    proto: Protocol,
    selected: ConnNamespace,
    pending: &mut VecDeque<PendingReply>,
    inflight: &mut usize,
) {
    let request = match parse_checkpoint_request(id, argv) {
        Ok(request) => request,
        Err(error) => {
            let mut reply = shared.take_reply_buf();
            RespWriter::new(&mut reply, proto).error(error);
            pending.push_back(PendingReply::Done(reply));
            return;
        }
    };
    let Some(target) = request.target else {
        push_local_checkpoint_reply(shared, request.mode, proto, pending);
        return;
    };
    if target.0 >= shared.cells {
        let mut reply = shared.take_reply_buf();
        RespWriter::new(&mut reply, proto).error("ERR INF.CKPT CELL target is out of range");
        pending.push_back(PendingReply::Done(reply));
        return;
    }
    if target.0 == shared.cell.0 {
        push_local_checkpoint_reply(shared, request.mode, proto, pending);
        return;
    }

    let remote_wait = [&b"INF.CKPT"[..], &b"WAIT"[..]];
    let remote_nowait = [&b"INF.CKPT"[..]];
    let remote: &[&[u8]] = if request.mode == CheckpointReplyMode::InfCkptWait {
        &remote_wait
    } else {
        &remote_nowait
    };
    match send_apply(shared, target, proto, selected, remote).await {
        Ok(waiter) => {
            *inflight += 1;
            pending.push_back(PendingReply::Remote { waiter, proto });
        }
        Err(refusal) => pending.push_back(PendingReply::Done(refusal)),
    }
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
    let Some((proto, id, selected)) =
        shared.with_conn(key, |c| (c.cx.proto, c.cx.id, c.cx.namespace))
    else {
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
        && shared.with_conn(key, |c| pubsub::subscriber_restricted(&c.cx)).unwrap_or(false)
        && !pubsub::is_plane_pubsub(meta.id)
    {
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
            pubsub::restricted_error(
                meta.id,
                meta.name,
                sub,
                &mut RespWriter::new(&mut reply, proto),
            );
        }
        pending.push_back(PendingReply::Done(reply));
        return true;
    }
    if let Some(meta) = meta
        && well_formed
        && shared.durable_write_refusal.get().is_some()
        && shared.durable_namespace_policy(meta, selected).is_some()
    {
        let mut reply = shared.take_reply_buf();
        assert!(
            shared.refuse_durable_write_into(origin, &argv, proto, &mut reply),
            "durable refusal checked above"
        );
        pending.push_back(PendingReply::Done(reply));
        return true;
    }
    let has_remote_key = |meta| {
        !shared.route_local_only
            && extract_keys_slices(meta, &argv)
                .iter()
                .any(|k| !shared.router.is_local(k, shared.cell))
    };
    let owner_of = |k: &[u8]| shared.router.cell_of(SlotRouter::slot_of(k));
    match meta {
        Some(meta) if well_formed && checkpoint_command(meta.id) => {
            dispatch_checkpoint_command(shared, meta.id, &argv, proto, selected, pending, inflight)
                .await;
        }
        Some(meta) if well_formed && pubsub::is_plane_pubsub(meta.id) => {
            return dispatch_pubsub(shared, key, meta.id, &argv, proto, pending, inflight).await;
        }
        Some(meta)
            if well_formed
                && !has_remote_key(meta)
                && let Some(durable) = shared.durable_namespace_policy(meta, selected) =>
        {
            let mut reply = shared.take_reply_buf();
            match durable.fsync {
                NsFsyncPolicy::Always => {
                    let assignment = shared.execute_durable_always_into(
                        origin, &argv, proto, selected, durable.id, &mut reply,
                    );
                    match assignment {
                        Some(assignment) => {
                            pending.push_back(PendingReply::DurableAlways { assignment, reply });
                        }
                        None => pending.push_back(PendingReply::Done(reply)),
                    }
                }
                NsFsyncPolicy::Everysec => {
                    shared.execute_durable_everysec_into(
                        origin, &argv, proto, selected, durable.id, &mut reply,
                    );
                    pending.push_back(PendingReply::Done(reply));
                }
            }
        }
        Some(meta)
            if well_formed
                && is_scatter(meta.id, argv.get(1).copied())
                && shared.cells > 1
                && !shared.route_local_only =>
        {
            match meta.id {
                CommandId::Dbsize => {
                    let acc = shared.apply_dbsize(origin, selected);
                    let mut waiters = Vec::new();
                    for cell in peer_cells(shared) {
                        if let Ok(waiter) =
                            send_apply(shared, cell, proto, selected, &[b"DBSIZE"]).await
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
                    let publish_catalog = namespace_catalog_ddl(meta.id, argv.get(1).copied())
                        && shared.ns_catalog_publish_enabled.get();
                    let local = run_local(shared, origin, proto, id, selected, &argv);
                    if local.first() == Some(&b'-') {
                        pending.push_back(PendingReply::Done(local));
                    } else {
                        shared.recycle_reply_buf(local);
                        let mut waiters = Vec::new();
                        for cell in peer_cells(shared) {
                            if let Ok(waiter) =
                                send_apply(shared, cell, proto, selected, &argv).await
                            {
                                waiters.push(waiter);
                                *inflight += 1;
                            }
                        }
                        if publish_catalog {
                            pending.push_back(PendingReply::AllOkThenNamespaceCatalog {
                                waiters,
                                proto,
                            });
                        } else {
                            pending.push_back(PendingReply::AllOk { waiters, proto });
                        }
                    }
                }
                CommandId::Keys => {
                    let reply = program_keys(shared, origin, proto, id, selected, &argv).await;
                    pending.push_back(PendingReply::Done(reply));
                }
                CommandId::Scan => {
                    let reply = program_scan(shared, origin, proto, id, selected, &argv).await;
                    pending.push_back(PendingReply::Done(reply));
                }
                CommandId::Randomkey => {
                    let reply = program_randomkey(shared, origin, proto, id, selected, &argv).await;
                    pending.push_back(PendingReply::Done(reply));
                }
                _ => unreachable!("is_scatter covers exactly the arms above"),
            }
        }
        Some(meta)
            if well_formed
                && namespace_catalog_ddl(meta.id, argv.get(1).copied())
                && shared.ns_catalog_publish_enabled.get() =>
        {
            let local = run_local(shared, origin, proto, id, selected, &argv);
            if local.first() == Some(&b'-') {
                pending.push_back(PendingReply::Done(local));
            } else {
                shared.recycle_reply_buf(local);
                let waiter = shared.queue_namespace_catalog_publish();
                pending.push_back(PendingReply::NamespaceCatalog { waiter, proto });
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
                    acc += shared.apply_counted(origin, name, k, selected);
                } else {
                    match send_apply(shared, owner_of(k), proto, selected, &[name, k]).await {
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
                    shared.execute_owned_into(origin, &[b"GET", k], proto, id, selected, &mut buf);
                    parts.push(GatherPart::Done(buf));
                } else {
                    match send_apply(shared, owner_of(k), proto, selected, &[b"GET", k]).await {
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
                            selected,
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
                                selected,
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
            let reply = program_msetnx(shared, origin, proto, id, selected, &argv).await;
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
            let reply = program_move(shared, origin, proto, id, selected, meta.id, &argv).await;
            pending.push_back(PendingReply::Done(reply));
        }
        Some(meta) if well_formed && has_remote_key(meta) => {
            // Single-owner remote command: ship the whole argv; the owner
            // executes and returns its raw RESP reply.
            let first_key = extract_keys_slices(meta, &argv)[0];
            let owner = owner_of(first_key);
            match send_apply(shared, owner, proto, selected, &argv).await {
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
                    namespace: c.cx.namespace,
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
                execute_slices(&argv, &mut shared.store.borrow_mut(), &mut live, now, &mut reply);
                shared.observer.borrow_mut().on_execute(shared.cell, origin, &argv, &reply, now);
                shared.with_conn(key, |c| {
                    c.cx.proto = live.proto;
                    c.cx.namespace = live.namespace;
                });
            } else {
                shared.execute_owned_into(origin, &argv, proto, id, selected, &mut reply);
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
    namespace: ConnNamespace,
    argv: &[&[u8]],
) -> Vec<u8> {
    let mut reply = shared.take_reply_buf();
    shared.execute_owned_into(origin, argv, proto, id, namespace, &mut reply);
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
    namespace: ConnNamespace,
    argv: &[&[u8]],
) -> Vec<u8> {
    if cell.0 == shared.cell.0 {
        return run_local(shared, origin, proto, id, namespace, argv);
    }
    match send_apply(shared, cell, proto, namespace, argv).await {
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
    namespace: ConnNamespace,
    name: &[u8],
    key: &[u8],
) -> Result<i64, Vec<u8>> {
    if cell.0 == shared.cell.0 {
        return Ok(shared.apply_counted(origin, name, key, namespace));
    }
    match send_apply(shared, cell, Protocol::Resp2, namespace, &[name, key]).await {
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
    namespace: ConnNamespace,
    argv: &[&[u8]],
) -> Vec<u8> {
    if argv.len().is_multiple_of(2) {
        return error_reply(shared, proto, "ERR wrong number of arguments for 'msetnx' command");
    }
    let mut i = 1;
    while i < argv.len() {
        let owner = shared.router.cell_of(SlotRouter::slot_of(argv[i]));
        match count_on(shared, origin, owner, proto, namespace, b"EXISTS", argv[i]).await {
            Ok(0) => {}
            Ok(_) => return int_reply(shared, proto, 0),
            Err(error) => return error,
        }
        i += 2;
    }
    let mut i = 1;
    while i < argv.len() {
        let owner = shared.router.cell_of(SlotRouter::slot_of(argv[i]));
        let reply = run_on(
            shared,
            origin,
            owner,
            Protocol::Resp2,
            id,
            namespace,
            &[b"SET", argv[i], argv[i + 1]],
        )
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
    namespace: ConnNamespace,
    cmd: CommandId,
    argv: &[&[u8]],
) -> Vec<u8> {
    let (src, dst) = (argv[1], argv[2]);
    let src_owner = shared.router.cell_of(SlotRouter::slot_of(src));
    let dst_owner = shared.router.cell_of(SlotRouter::slot_of(dst));
    let mut replace = false;
    let mut dst_namespace = namespace;
    if cmd == CommandId::Copy {
        let mut i = 3;
        while i < argv.len() {
            let opt = argv[i];
            if opt.eq_ignore_ascii_case(b"REPLACE") {
                replace = true;
            } else if opt.eq_ignore_ascii_case(b"DB") && i + 1 < argv.len() {
                if namespace.selected_db().is_none() {
                    return error_reply(
                        shared,
                        proto,
                        "ERR COPY DB is only valid from default namespaces",
                    );
                }
                match crate::exec::parse_i64(argv[i + 1]) {
                    Ok(n @ 0..=15) => dst_namespace = ConnNamespace::default_db(n as u16),
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
        match count_on(shared, origin, dst_owner, proto, namespace, b"EXISTS", dst).await {
            Ok(0) => {}
            Ok(_) => return int_reply(shared, proto, 0),
            Err(error) => return error,
        }
    }
    let probe: &[u8] = if cmd == CommandId::Copy { b"INF.PEEK" } else { b"INF.TAKE" };
    let raw =
        run_on(shared, origin, src_owner, Protocol::Resp2, id, namespace, &[probe, src]).await;
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
    // COPY's destination default database or named namespace rides Apply v1.
    let reply = run_on(shared, origin, dst_owner, Protocol::Resp2, id, dst_namespace, &put).await;
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
    namespace: ConnNamespace,
    argv: &[&[u8]],
) -> Vec<u8> {
    let local = run_local(shared, origin, proto, id, namespace, argv);
    let Some((mut total, local_off)) = parse_array_header(&local) else {
        return local; // error passthrough
    };
    let mut waiters = Vec::new();
    for cell in peer_cells(shared) {
        match send_apply(shared, cell, proto, namespace, argv).await {
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
    namespace: ConnNamespace,
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
    let raw = run_on(shared, origin, CellId(target), proto, id, namespace, &sub).await;
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
    namespace: ConnNamespace,
    argv: &[&[u8]],
) -> Vec<u8> {
    let start = (crate::exec::next_rand(&shared.node) % u64::from(shared.cells)) as u16;
    for i in 0..shared.cells {
        let cell = CellId((start + i) % shared.cells);
        let raw = run_on(shared, origin, cell, proto, id, namespace, argv).await;
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
async fn dispatch_pubsub<O: PlaneObserver + 'static>(
    shared: &Rc<Shared<O>>,
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
                    if let Ok(waiter) = send_apply(
                        shared,
                        CellId(cell),
                        Protocol::Resp2,
                        ConnNamespace::default(),
                        fan,
                    )
                    .await
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
                match send_apply(shared, owner, Protocol::Resp2, ConnNamespace::default(), publ)
                    .await
                {
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
async fn send_sub_delta<O: PlaneObserver + 'static>(
    shared: &Rc<Shared<O>>,
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
            } else if let Ok(waiter) =
                send_apply(shared, owner, Protocol::Resp2, ConnNamespace::default(), subd).await
            {
                waiters.push(waiter);
            }
        }
        SubKind::Pattern => {
            shared.pubsub.borrow_mut().apply_delta(kind, name, shared.cell.0, delta);
            if !shared.route_local_only {
                for cell in peer_cells(shared) {
                    if let Ok(waiter) =
                        send_apply(shared, cell, Protocol::Resp2, ConnNamespace::default(), subd)
                            .await
                    {
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
async fn flush_sub_deltas<O: PlaneObserver + 'static>(
    shared: Rc<Shared<O>>,
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
fn unsubscribe_closed_conn<O: PlaneObserver + 'static>(
    shared: &Rc<Shared<O>>,
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
async fn owner_pub_pump<O: PlaneObserver + 'static>(shared: Rc<Shared<O>>) {
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
            if let Ok(waiter) =
                send_apply(&shared, CellId(cell), Protocol::Resp2, ConnNamespace::default(), fan)
                    .await
            {
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

async fn owner_durable_reply_pump<O: PlaneObserver + 'static>(shared: Rc<Shared<O>>) {
    loop {
        let Some(item) = shared.owner_durable_replies.borrow_mut().pop_front() else {
            shared.owner_durable_pump_active.set(false);
            return;
        };
        let watermark_key = item.assignment.await;
        shared.durability_gate.waiter(watermark_key).await;
        let mut fabric = shared.fabric.borrow_mut();
        match &item.body {
            OwnerDurableReplyBody::Bytes(bytes) => {
                fabric.reply(item.origin, item.token, &Outcome::Bytes(bytes));
            }
            OwnerDurableReplyBody::Int(n) => {
                fabric.reply(item.origin, item.token, &Outcome::Int(*n));
            }
        }
        fabric.flush();
    }
}

async fn owner_checkpoint_reply_pump<O: PlaneObserver + 'static>(shared: Rc<Shared<O>>) {
    loop {
        let Some(item) = shared.owner_checkpoint_replies.borrow_mut().pop_front() else {
            shared.owner_checkpoint_pump_active.set(false);
            return;
        };
        item.waiter.await;
        let mut fabric = shared.fabric.borrow_mut();
        fabric.reply(item.origin, item.token, &Outcome::Bytes(b"+OK\r\n"));
        fabric.flush();
    }
}

/// PUBSUB introspection over the cell registries: CHANNELS merges the owner
/// views (KEYS-style header arithmetic), NUMSUB asks each channel's owner,
/// NUMPAT answers locally (the pattern index is replicated).
async fn program_pubsub<O: PlaneObserver + 'static>(
    shared: &Rc<Shared<O>>,
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
                match send_apply(shared, cell, Protocol::Resp2, ConnNamespace::default(), &request)
                    .await
                {
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
                match send_apply(shared, owner, Protocol::Resp2, ConnNamespace::default(), numsub)
                    .await
                {
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
fn handle_pubsub_apply<O: PlaneObserver + 'static>(
    shared: &Shared<O>,
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
fn deliver_local<O: PlaneObserver + 'static>(
    shared: &Shared<O>,
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

fn deliver_one<O: PlaneObserver + 'static>(
    shared: &Shared<O>,
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
    namespace: ConnNamespace,
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
    // Apply v1 carries the selected namespace id explicitly; `cmd` stays the
    // protocol byte so named namespaces do not consume argv space.
    let proto_byte: u8 = match proto {
        Protocol::Resp3 => 3,
        Protocol::Resp2 => 2,
    };
    loop {
        let op = Op::Apply { token, slot, namespace: namespace.id(), cmd: proto_byte, args };
        let sent = shared.fabric.borrow_mut().send(to, &op);
        match sent {
            Ok(()) => break,
            Err(SendError::NoCredit { .. }) => shared.credit_waiters.wait(to).await,
        }
    }
    // RTT pairing relies on in-order replies; `INF.PUB` replies are deferred
    // by the owner pump (fan acks first), so its hops are not RTT samples —
    // the fan legs (`INF.PUBFAN`) cover pub/sub in the histogram instead.
    if argv.first().copied() != Some(&b"INF.PUB"[..]) {
        shared.rtt_sent.borrow_mut()[usize::from(to.0)].push_back((token.0, shared.now.get()));
    }
    Ok(waiter)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::io;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::time::Duration;

    use inf_alloc::{BufferPool, LeaseKind};
    use inf_fabric::{Mesh, MeshConfig};
    use inf_foundation::time::{Nanos, VirtualClock};
    use inf_log::{NamespaceId, SegmentConfig, SegmentId, decode_batch_frame};
    use inf_runtime::{
        BackendDriver, Capabilities, CellExecutor, CellLoop, FileOpenMode, LoopConfig,
        PollImmediate, SubmitStats, Wait,
    };
    use inf_store::{MutationEffect, StoreConfig};

    use crate::checkpoint::LiveCheckpointPublishConfig;
    use crate::log_maintenance::LogSegmentMaintenanceConfig;

    const ACTIVE_LOG_FD: RawFd = 42;
    const LOG_TOKEN_SLOT: u32 = 17;
    const LOG_TOKEN_GENERATION: u32 = 9;
    const NS_CATALOG_TOKEN_SLOT: u32 = 18;
    const NS_CATALOG_TOKEN_GENERATION: u32 = 10;
    const NS_ROOT_FD: RawFd = 50;
    const NS_TEMP_FD: RawFd = 51;
    const LOG_MAINTENANCE_TOKEN_SLOT: u32 = 19;
    const LOG_MAINTENANCE_TOKEN_GENERATION: u32 = 11;
    const LOG_DIR_FD: RawFd = 52;
    const PREPARED_LOG_FD: RawFd = 53;
    const CHECKPOINT_TOKEN_SLOT: u32 = 20;
    const CHECKPOINT_TOKEN_GENERATION: u32 = 12;
    const CHECKPOINT_DIR_FD: RawFd = 54;
    const CHECKPOINT_TEMP_FD: RawFd = 55;
    const TEST_EIO: i32 = 5;

    #[derive(Debug, Default)]
    struct DriverState {
        pushed: Vec<IoOp>,
        completions: VecDeque<Completion>,
        recv_payloads: VecDeque<(CompletionToken, Vec<u8>)>,
        pending_sqes: u64,
        stats: SubmitStats,
        lease_one_on_submit: bool,
        release_held_on_submit: bool,
        held: Option<BufferId>,
    }

    #[derive(Clone, Debug)]
    struct TestDriver {
        state: Rc<RefCell<DriverState>>,
    }

    impl TestDriver {
        fn new(state: Rc<RefCell<DriverState>>) -> TestDriver {
            TestDriver { state }
        }
    }

    impl BackendDriver for TestDriver {
        fn push(&mut self, op: IoOp) {
            let mut state = self.state.borrow_mut();
            state.pending_sqes += 1;
            state.pushed.push(op);
        }

        fn submit_and_reap(
            &mut self,
            pool: &mut BufferPool,
            _wait: Wait,
            out: &mut Vec<Completion>,
        ) -> io::Result<usize> {
            let mut state = self.state.borrow_mut();
            if state.release_held_on_submit {
                if let Some(buf) = state.held.take() {
                    pool.release(buf);
                }
                state.release_held_on_submit = false;
            }
            if state.lease_one_on_submit {
                state.held = pool.try_lease(LeaseKind::Send);
                assert!(state.held.is_some(), "test expected one buffer to lease");
                state.lease_one_on_submit = false;
            }

            let reaped = state.completions.len();
            out.extend(state.completions.drain(..));
            let mut reaped = reaped;
            while let Some((token, payload)) = state.recv_payloads.pop_front() {
                assert!(payload.len() <= pool.buf_size(), "test recv payload exceeds buffer size");
                let Some(buf) = pool.try_lease(LeaseKind::Recv) else {
                    state.recv_payloads.push_front((token, payload));
                    break;
                };
                pool.bytes_mut(buf)[..payload.len()].copy_from_slice(&payload);
                out.push(Completion {
                    token,
                    result: CompletionResult::Recv { buf, len: payload.len() as u32 },
                });
                reaped += 1;
            }
            state.stats =
                SubmitStats { syscalls: 1, sqes: state.pending_sqes, cqes: reaped as u64 };
            state.pending_sqes = 0;
            Ok(reaped)
        }

        fn register_pool(&mut self, _pool: &mut BufferPool) -> io::Result<()> {
            Ok(())
        }

        fn capabilities(&self) -> Capabilities {
            Capabilities {
                backend: "log-test",
                multishot_accept: false,
                multishot_recv: false,
                provided_buffers: false,
                fixed_buffers: false,
                single_issuer: false,
                defer_taskrun: false,
                performance_tier: false,
            }
        }

        fn submit_stats(&self) -> SubmitStats {
            self.state.borrow().stats
        }
    }

    fn log_token() -> CompletionToken {
        CompletionToken::new(TokenClass::File, LOG_TOKEN_SLOT, LOG_TOKEN_GENERATION)
    }

    fn log_writer() -> LogWriteIo {
        let config = SegmentConfig::new(4096, 1024, 1024).unwrap();
        LogWriteIo::open(
            ACTIVE_LOG_FD,
            SegmentId::ZERO,
            0,
            config,
            LOG_TOKEN_SLOT,
            LOG_TOKEN_GENERATION,
        )
        .unwrap()
    }

    fn rotating_log_writer() -> LogWriteIo {
        let config = SegmentConfig::new(128, 96, 96).unwrap();
        LogWriteIo::open(
            ACTIVE_LOG_FD,
            SegmentId::ZERO,
            80,
            config,
            LOG_TOKEN_SLOT,
            LOG_TOKEN_GENERATION,
        )
        .unwrap()
    }

    fn log_maintenance_token() -> CompletionToken {
        CompletionToken::new(
            TokenClass::File,
            LOG_MAINTENANCE_TOKEN_SLOT,
            LOG_MAINTENANCE_TOKEN_GENERATION,
        )
    }

    fn log_segment_maintenance() -> LogSegmentMaintenance {
        LogSegmentMaintenance::new(
            LogSegmentMaintenanceConfig::new(LOG_DIR_FD, LOG_MAINTENANCE_TOKEN_SLOT)
                .with_generation(LOG_MAINTENANCE_TOKEN_GENERATION),
        )
    }

    fn ns_catalog_token() -> CompletionToken {
        CompletionToken::new(TokenClass::File, NS_CATALOG_TOKEN_SLOT, NS_CATALOG_TOKEN_GENERATION)
    }

    fn ns_catalog_publisher() -> NamespaceCatalogLivePublisher {
        let config = crate::ns_catalog::NamespaceCatalogLivePublishConfig::new(
            "infinity-data".to_string(),
            NS_CATALOG_TOKEN_SLOT,
        )
        .with_generation(NS_CATALOG_TOKEN_GENERATION);
        NamespaceCatalogLivePublisher::new(config).unwrap()
    }

    fn checkpoint_token() -> CompletionToken {
        CompletionToken::new(TokenClass::File, CHECKPOINT_TOKEN_SLOT, CHECKPOINT_TOKEN_GENERATION)
    }

    fn checkpoint_publisher() -> LiveCheckpointPublisher {
        LiveCheckpointPublisher::new(
            LiveCheckpointPublishConfig::new(CHECKPOINT_DIR_FD, CHECKPOINT_TOKEN_SLOT)
                .with_generation(CHECKPOINT_TOKEN_GENERATION),
        )
    }

    fn test_plane() -> ServerPlane {
        let fabric = Mesh::new(1, MeshConfig { ring_capacity: 64, data_credits: 16 })
            .into_iter()
            .next()
            .unwrap();
        test_plane_with_fabric(CellId(0), 1, fabric)
    }

    fn test_plane_with_fabric(cell: CellId, cells: u16, fabric: CellFabric) -> ServerPlane {
        let mut plane = ServerPlane::new(
            cell,
            cells,
            123,
            Keyspace::new(StoreConfig::default()),
            fabric,
            Rc::new(NodeInfo::default()),
            NoopObserver,
            false,
        );
        plane.started = true;
        plane
    }

    fn test_loop(
        state: Rc<RefCell<DriverState>>,
        buffers: usize,
    ) -> CellLoop<TestDriver, Rc<VirtualClock>> {
        let clock = Rc::new(VirtualClock::new(Nanos::ZERO));
        test_loop_with_clock(state, buffers, clock)
    }

    fn test_loop_with_clock(
        state: Rc<RefCell<DriverState>>,
        buffers: usize,
        clock: Rc<VirtualClock>,
    ) -> CellLoop<TestDriver, Rc<VirtualClock>> {
        let config = LoopConfig {
            spin_iters: 0,
            park_default: Some(Duration::from_millis(0)),
            ..Default::default()
        };
        CellLoop::new(TestDriver::new(state), clock, BufferPool::new(buffers, 1024), config)
    }

    fn resp_command(parts: &[&[u8]]) -> Vec<u8> {
        let mut wire = format!("*{}\r\n", parts.len()).into_bytes();
        for part in parts {
            wire.extend_from_slice(format!("${}\r\n", part.len()).as_bytes());
            wire.extend_from_slice(part);
            wire.extend_from_slice(b"\r\n");
        }
        wire
    }

    fn insert_test_conn(plane: &ServerPlane, fd: RawFd) -> ConnKey {
        let key = plane.shared.conns.borrow_mut().insert(Conn {
            fd,
            parser: ConnParser::new(ParserLimits::default()),
            cx: ConnCx {
                proto: Protocol::Resp2,
                id: 0,
                namespace: ConnNamespace::default(),
                sub_channels: Vec::new(),
                sub_patterns: Vec::new(),
                node: Rc::clone(&plane.shared.node),
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
        plane.shared.with_conn(key, |conn| conn.cx.id = id);
        key
    }

    fn queue_recv(state: &Rc<RefCell<DriverState>>, key: ConnKey, parts: &[&[u8]]) {
        let token = ServerPlane::<NoopObserver>::token(TokenClass::Recv, key);
        state.borrow_mut().recv_payloads.push_back((token, resp_command(parts)));
    }

    fn queue_recv_many(state: &Rc<RefCell<DriverState>>, key: ConnKey, commands: &[&[&[u8]]]) {
        let token = ServerPlane::<NoopObserver>::token(TokenClass::Recv, key);
        let mut wire = Vec::new();
        for parts in commands {
            wire.extend_from_slice(&resp_command(parts));
        }
        state.borrow_mut().recv_payloads.push_back((token, wire));
    }

    fn conn_out(plane: &ServerPlane, key: ConnKey) -> Vec<u8> {
        plane.shared.with_conn(key, |conn| conn.out.clone()).expect("test connection exists")
    }

    fn clear_conn_out(plane: &ServerPlane, key: ConnKey) {
        plane.shared.with_conn(key, |conn| conn.out.clear()).expect("test connection exists");
    }

    fn submit_one_checkpoint_op(
        cell_loop: &mut CellLoop<TestDriver, Rc<VirtualClock>>,
        plane: &mut ServerPlane,
        state: &Rc<RefCell<DriverState>>,
    ) -> IoOp {
        state.borrow_mut().pushed.clear();
        cell_loop.run_iteration(plane).unwrap();
        let mut state = state.borrow_mut();
        let ops: Vec<IoOp> = state
            .pushed
            .drain(..)
            .filter(|op| match op {
                IoOp::FileOpen { token, .. }
                | IoOp::FileWriteAt { token, .. }
                | IoOp::FileSync { token, .. }
                | IoOp::FileClose { token, .. }
                | IoOp::FileRename { token, .. }
                | IoOp::FileUnlink { token, .. }
                | IoOp::FileTruncate { token, .. }
                | IoOp::FilePreallocate { token, .. } => *token == checkpoint_token(),
                _ => false,
            })
            .collect();
        assert_eq!(ops.len(), 1, "expected exactly one checkpoint op");
        ops.into_iter().next().unwrap()
    }

    fn completion_for_checkpoint_op(op: &IoOp) -> Completion {
        match op {
            IoOp::FileOpen { token, .. } => Completion {
                token: *token,
                result: CompletionResult::FileOpened { fd: CHECKPOINT_TEMP_FD },
            },
            IoOp::FileWriteAt { buf, token, .. } => {
                Completion { token: *token, result: CompletionResult::FileWritten { buf: *buf } }
            }
            IoOp::FileSync { token, .. } | IoOp::FileRename { token, .. } => {
                Completion { token: *token, result: CompletionResult::FileDone }
            }
            IoOp::FileClose { token, .. } => {
                Completion { token: *token, result: CompletionResult::FileClosed }
            }
            other => panic!("unexpected checkpoint op {other:?}"),
        }
    }

    fn complete_live_checkpoint_publish(
        cell_loop: &mut CellLoop<TestDriver, Rc<VirtualClock>>,
        plane: &mut ServerPlane,
        state: &Rc<RefCell<DriverState>>,
    ) {
        state.borrow_mut().pushed.clear();
        cell_loop.run_iteration(plane).unwrap();
        let (buf, token, _len) = one_log_write(state);
        state
            .borrow_mut()
            .completions
            .push_back(Completion { token, result: CompletionResult::FileWritten { buf } });

        cell_loop.run_iteration(plane).unwrap();
        cell_loop.run_iteration(plane).unwrap();
        let sync_token = one_log_sync(state);
        state
            .borrow_mut()
            .completions
            .push_back(Completion { token: sync_token, result: CompletionResult::FileDone });
        cell_loop.run_iteration(plane).unwrap();

        let mut saw_manifest_rename = false;
        for _ in 0..16 {
            let op = submit_one_checkpoint_op(cell_loop, plane, state);
            if let IoOp::FileRename { new_name, .. } = &op
                && new_name == inf_log::RECOVERY_MANIFEST_FILE
            {
                saw_manifest_rename = true;
            }
            let completion = completion_for_checkpoint_op(&op);
            state.borrow_mut().completions.push_back(completion);
            cell_loop.run_iteration(plane).unwrap();
            if plane.shared.node.checkpoint_in_progress.get() == 0 {
                break;
            }
        }

        assert!(saw_manifest_rename, "test must drive the MANIFEST rename");
        assert_eq!(plane.shared.node.checkpoint_in_progress.get(), 0);
    }

    fn stage_delete(plane: &ServerPlane, key: &'static [u8]) {
        plane
            .shared
            .durability
            .borrow_mut()
            .stage_mutation_effect(NamespaceId::new(1), MutationEffect::Delete { key })
            .unwrap();
    }

    fn stage_delete_always(plane: &ServerPlane, key: &'static [u8]) -> GateWait<u64, u64> {
        stage_delete(plane, key);
        plane.shared.register_durability_assignment()
    }

    fn one_log_write(state: &Rc<RefCell<DriverState>>) -> (BufferId, CompletionToken, u32) {
        let state = state.borrow();
        let writes: Vec<_> = state
            .pushed
            .iter()
            .filter_map(|op| match op {
                IoOp::FileWriteAt { fd, offset_bytes, buf, len, token } => {
                    assert_eq!(*fd, ACTIVE_LOG_FD);
                    assert_eq!(*offset_bytes, 0);
                    assert_eq!(*token, log_token());
                    Some((*buf, *token, *len))
                }
                _ => None,
            })
            .collect();
        assert_eq!(writes.len(), 1, "expected exactly one log file write: {:?}", state.pushed);
        writes[0]
    }

    fn one_log_sync(state: &Rc<RefCell<DriverState>>) -> CompletionToken {
        let state = state.borrow();
        let syncs: Vec<_> = state
            .pushed
            .iter()
            .filter_map(|op| match op {
                IoOp::FileSync { fd, mode, token } => {
                    assert_eq!(*fd, ACTIVE_LOG_FD);
                    assert_eq!(*mode, FileSyncMode::DataOnly);
                    assert_eq!(*token, log_token());
                    Some(*token)
                }
                _ => None,
            })
            .collect();
        assert_eq!(syncs.len(), 1, "expected exactly one log file sync: {:?}", state.pushed);
        syncs[0]
    }

    fn log_sync_count(state: &Rc<RefCell<DriverState>>) -> usize {
        state
            .borrow()
            .pushed
            .iter()
            .filter(|op| {
                matches!(
                    op,
                    IoOp::FileSync {
                        fd: ACTIVE_LOG_FD,
                        mode: FileSyncMode::DataOnly,
                        token,
                    } if *token == log_token()
                )
            })
            .count()
    }

    fn submit_one_log_writer_op(
        cell_loop: &mut CellLoop<TestDriver, Rc<VirtualClock>>,
        plane: &mut ServerPlane,
        state: &Rc<RefCell<DriverState>>,
    ) -> IoOp {
        state.borrow_mut().pushed.clear();
        cell_loop.run_iteration(plane).unwrap();
        cell_loop.run_iteration(plane).unwrap();
        let mut state = state.borrow_mut();
        let pushed_debug = format!("{:?}", state.pushed);
        let ops: Vec<IoOp> = state
            .pushed
            .drain(..)
            .filter(|op| match op {
                IoOp::FileTruncate { token, .. }
                | IoOp::FileWriteAt { token, .. }
                | IoOp::FileSync { token, .. } => *token == log_token(),
                _ => false,
            })
            .collect();
        assert_eq!(ops.len(), 1, "expected exactly one log writer op, pushed {pushed_debug}");
        ops.into_iter().next().unwrap()
    }

    fn submit_one_catalog_op(
        cell_loop: &mut CellLoop<TestDriver, Rc<VirtualClock>>,
        plane: &mut ServerPlane,
        state: &Rc<RefCell<DriverState>>,
    ) -> IoOp {
        state.borrow_mut().pushed.clear();
        cell_loop.run_iteration(plane).unwrap();
        let mut state = state.borrow_mut();
        let ops: Vec<IoOp> = state
            .pushed
            .drain(..)
            .filter(|op| match op {
                IoOp::FileOpen { token, .. }
                | IoOp::FileWriteAt { token, .. }
                | IoOp::FileSync { token, .. }
                | IoOp::FileClose { token, .. }
                | IoOp::FileRename { token, .. } => *token == ns_catalog_token(),
                _ => false,
            })
            .collect();
        assert_eq!(ops.len(), 1, "expected exactly one catalog op");
        ops.into_iter().next().unwrap()
    }

    fn submit_one_log_maintenance_op(
        cell_loop: &mut CellLoop<TestDriver, Rc<VirtualClock>>,
        plane: &mut ServerPlane,
        state: &Rc<RefCell<DriverState>>,
    ) -> IoOp {
        state.borrow_mut().pushed.clear();
        cell_loop.run_iteration(plane).unwrap();
        cell_loop.run_iteration(plane).unwrap();
        let mut state = state.borrow_mut();
        let pushed_debug = format!("{:?}", state.pushed);
        let ops: Vec<IoOp> = state
            .pushed
            .drain(..)
            .filter(|op| match op {
                IoOp::FileOpen { token, .. }
                | IoOp::FilePreallocate { token, .. }
                | IoOp::FileSync { token, .. } => *token == log_maintenance_token(),
                _ => false,
            })
            .collect();
        assert_eq!(ops.len(), 1, "expected exactly one log maintenance op, pushed {pushed_debug}");
        ops.into_iter().next().unwrap()
    }

    fn complete_one_log_maintenance_op(state: &Rc<RefCell<DriverState>>, op: IoOp) {
        let completion = match op {
            IoOp::FileOpen { dir, name, mode, token } => {
                assert_eq!(dir, LOG_DIR_FD);
                assert_eq!(name, SegmentId::new(1).unwrap().file_name());
                assert_eq!(mode, FileOpenMode::ReadWriteCreate);
                assert_eq!(token, log_maintenance_token());
                Completion { token, result: CompletionResult::FileOpened { fd: PREPARED_LOG_FD } }
            }
            IoOp::FilePreallocate { fd, len_bytes, token } => {
                assert_eq!(fd, PREPARED_LOG_FD);
                assert_eq!(len_bytes, 128);
                assert_eq!(token, log_maintenance_token());
                Completion { token, result: CompletionResult::FileDone }
            }
            IoOp::FileSync { fd, mode, token } => {
                assert_eq!(token, log_maintenance_token());
                match mode {
                    FileSyncMode::DataOnly => assert_eq!(fd, PREPARED_LOG_FD),
                    FileSyncMode::Full => assert_eq!(fd, LOG_DIR_FD),
                }
                Completion { token, result: CompletionResult::FileDone }
            }
            other => panic!("unexpected maintenance op {other:?}"),
        };
        state.borrow_mut().completions.push_back(completion);
    }

    fn completion_for_catalog_op(op: IoOp) -> Completion {
        match op {
            IoOp::FileOpen { dir, name, mode, token } => {
                assert_eq!(token, ns_catalog_token());
                match mode {
                    FileOpenMode::Directory => {
                        assert_eq!(name, "infinity-data");
                        Completion {
                            token,
                            result: CompletionResult::FileOpened { fd: NS_ROOT_FD },
                        }
                    }
                    FileOpenMode::ReadWriteCreateTruncate => {
                        assert_eq!(dir, NS_ROOT_FD);
                        assert_eq!(name, crate::ns_catalog::NAMESPACE_CATALOG_TEMP_FILE);
                        Completion {
                            token,
                            result: CompletionResult::FileOpened { fd: NS_TEMP_FD },
                        }
                    }
                    other => panic!("unexpected catalog open mode {other:?}"),
                }
            }
            IoOp::FileWriteAt { fd, offset_bytes, buf, len, token } => {
                assert_eq!(fd, NS_TEMP_FD);
                assert_eq!(offset_bytes, 0);
                assert!(len > 0);
                assert_eq!(token, ns_catalog_token());
                Completion { token, result: CompletionResult::FileWritten { buf } }
            }
            IoOp::FileSync { fd, mode, token } => {
                assert_eq!(token, ns_catalog_token());
                match mode {
                    FileSyncMode::DataOnly => assert_eq!(fd, NS_TEMP_FD),
                    FileSyncMode::Full => assert_eq!(fd, NS_ROOT_FD),
                }
                Completion { token, result: CompletionResult::FileDone }
            }
            IoOp::FileClose { fd, token } => {
                assert_eq!(token, ns_catalog_token());
                assert!(fd == NS_TEMP_FD || fd == NS_ROOT_FD);
                Completion { token, result: CompletionResult::FileClosed }
            }
            IoOp::FileRename { old_dir, old_name, new_dir, new_name, token } => {
                assert_eq!(token, ns_catalog_token());
                assert_eq!(old_dir, NS_ROOT_FD);
                assert_eq!(new_dir, NS_ROOT_FD);
                assert_eq!(old_name, crate::ns_catalog::NAMESPACE_CATALOG_TEMP_FILE);
                assert_eq!(new_name, crate::ns_catalog::NAMESPACE_CATALOG_FILE);
                Completion { token, result: CompletionResult::FileDone }
            }
            other => panic!("unexpected catalog op {other:?}"),
        }
    }

    #[test]
    fn namespace_catalog_reply_waits_for_live_meta_publish() {
        let state = Rc::new(RefCell::new(DriverState::default()));
        let mut cell_loop = test_loop(Rc::clone(&state), 4);
        let mut plane = test_plane();
        plane.install_namespace_catalog_publisher(ns_catalog_publisher());
        let key = insert_test_conn(&plane, 99);
        queue_recv(&state, key, &[b"INF.NS", b"CREATE", b"cache"]);

        cell_loop.run_iteration(&mut plane).unwrap();
        assert!(conn_out(&plane, key).is_empty(), "reply escaped before catalog publish");

        for step in 0..8 {
            let op = submit_one_catalog_op(&mut cell_loop, &mut plane, &state);
            let completion = completion_for_catalog_op(op);
            if step == 7 {
                plane.shared.with_conn(key, |conn| conn.send_inflight = true);
            }
            state.borrow_mut().completions.push_back(completion);
            cell_loop.run_iteration(&mut plane).unwrap();
            if step < 7 {
                assert!(conn_out(&plane, key).is_empty(), "reply escaped before META commit");
            }
        }

        assert_eq!(conn_out(&plane, key), b"+OK\r\n");
        assert!(plane.shared.store.borrow().ns_get(b"cache").is_some());
        assert_eq!(plane.shared.ns_catalog_publish_gate.pending(), 0);
        assert_eq!(cell_loop.pool().reconcile(), Ok(()));
    }

    #[test]
    fn namespace_catalog_publish_error_fail_stops_after_write_buffer_return() {
        let state = Rc::new(RefCell::new(DriverState::default()));
        let mut cell_loop = test_loop(Rc::clone(&state), 4);
        let mut plane = test_plane();
        plane.install_namespace_catalog_publisher(ns_catalog_publisher());
        let key = insert_test_conn(&plane, 99);
        queue_recv(&state, key, &[b"INF.NS", b"CREATE", b"cache"]);

        cell_loop.run_iteration(&mut plane).unwrap();
        let op = submit_one_catalog_op(&mut cell_loop, &mut plane, &state);
        state.borrow_mut().completions.push_back(completion_for_catalog_op(op));
        cell_loop.run_iteration(&mut plane).unwrap();
        let op = submit_one_catalog_op(&mut cell_loop, &mut plane, &state);
        state.borrow_mut().completions.push_back(completion_for_catalog_op(op));
        cell_loop.run_iteration(&mut plane).unwrap();
        let op = submit_one_catalog_op(&mut cell_loop, &mut plane, &state);
        let IoOp::FileWriteAt { buf, token, .. } = op else { panic!("expected catalog write op") };
        state.borrow_mut().completions.push_back(Completion {
            token,
            result: CompletionResult::Error { errno: TEST_EIO, buf: Some(buf) },
        });

        let result = catch_unwind(AssertUnwindSafe(|| cell_loop.run_iteration(&mut plane)));

        assert!(result.is_err(), "catalog write error must fail-stop");
        assert_eq!(cell_loop.pool().reconcile(), Ok(()));
        assert!(conn_out(&plane, key).is_empty());
    }

    #[test]
    fn checkpoint_cell_parser_accepts_target_and_wait_order() {
        assert_eq!(
            parse_checkpoint_request(
                CommandId::InfCkpt,
                &[&b"INF.CKPT"[..], &b"CELL"[..], &b"1"[..], &b"WAIT"[..]]
            )
            .unwrap(),
            CheckpointRequest { mode: CheckpointReplyMode::InfCkptWait, target: Some(CellId(1)) }
        );
        assert_eq!(
            parse_checkpoint_request(
                CommandId::InfCkpt,
                &[&b"INF.CKPT"[..], &b"WAIT"[..], &b"CELL"[..], &b"0"[..]]
            )
            .unwrap(),
            CheckpointRequest { mode: CheckpointReplyMode::InfCkptWait, target: Some(CellId(0)) }
        );
        assert!(
            parse_checkpoint_request(CommandId::InfCkpt, &[&b"INF.CKPT"[..], b"CELL", b"two"])
                .is_err()
        );
        assert!(
            parse_checkpoint_request(
                CommandId::InfCkpt,
                &[&b"INF.CKPT"[..], b"CELL", b"1", b"CELL", b"0"]
            )
            .is_err()
        );
    }

    #[test]
    fn checkpoint_wait_replies_after_manifest_publish() {
        let state = Rc::new(RefCell::new(DriverState::default()));
        let mut cell_loop = test_loop(Rc::clone(&state), 8);
        let mut plane = test_plane();
        plane.install_log_writer(log_writer());
        plane.install_checkpoint_publisher(checkpoint_publisher());
        let key = insert_test_conn(&plane, 99);
        plane.shared.with_conn(key, |conn| conn.send_inflight = true);
        queue_recv(&state, key, &[b"INF.CKPT", b"WAIT"]);

        cell_loop.run_iteration(&mut plane).unwrap();
        assert!(conn_out(&plane, key).is_empty(), "checkpoint WAIT replied before LOG");
        assert_eq!(plane.shared.node.checkpoint_in_progress.get(), 1);

        cell_loop.run_iteration(&mut plane).unwrap();
        let (buf, token, len) = one_log_write(&state);
        let watermark_key = log_watermark_key(Lsn::new(0, len));
        assert!(conn_out(&plane, key).is_empty(), "checkpoint WAIT replied before log write");

        state
            .borrow_mut()
            .completions
            .push_back(Completion { token, result: CompletionResult::FileWritten { buf } });
        cell_loop.run_iteration(&mut plane).unwrap();
        assert!(conn_out(&plane, key).is_empty(), "checkpoint WAIT replied before log fsync op");

        cell_loop.run_iteration(&mut plane).unwrap();
        let sync_token = one_log_sync(&state);
        assert!(conn_out(&plane, key).is_empty(), "checkpoint WAIT replied before log fsync");

        state
            .borrow_mut()
            .completions
            .push_back(Completion { token: sync_token, result: CompletionResult::FileDone });
        cell_loop.run_iteration(&mut plane).unwrap();
        assert_eq!(plane.durability_watermark(), watermark_key);
        assert!(conn_out(&plane, key).is_empty(), "checkpoint WAIT replied before ckpt files");

        let mut saw_manifest_rename = false;
        for _ in 0..16 {
            let op = submit_one_checkpoint_op(&mut cell_loop, &mut plane, &state);
            if let IoOp::FileRename { new_name, .. } = &op
                && new_name == inf_log::RECOVERY_MANIFEST_FILE
            {
                saw_manifest_rename = true;
            }
            let completion = completion_for_checkpoint_op(&op);
            state.borrow_mut().completions.push_back(completion);
            cell_loop.run_iteration(&mut plane).unwrap();
            if conn_out(&plane, key) == b"+OK\r\n" {
                break;
            }
        }

        assert!(saw_manifest_rename, "test must drive the MANIFEST rename");
        assert_eq!(conn_out(&plane, key), b"+OK\r\n");
        assert_eq!(plane.shared.node.checkpoint_in_progress.get(), 0);
        assert_eq!(plane.shared.checkpoint_publish_gate.pending(), 0);
        assert_eq!(cell_loop.pool().reconcile(), Ok(()));
    }

    #[test]
    fn checkpoint_cell_local_wait_replies_after_manifest_publish() {
        let state = Rc::new(RefCell::new(DriverState::default()));
        let mut cell_loop = test_loop(Rc::clone(&state), 8);
        let mut plane = test_plane();
        plane.install_log_writer(log_writer());
        plane.install_checkpoint_publisher(checkpoint_publisher());
        let key = insert_test_conn(&plane, 99);
        plane.shared.with_conn(key, |conn| conn.send_inflight = true);
        queue_recv(&state, key, &[b"INF.CKPT", b"CELL", b"0", b"WAIT"]);

        cell_loop.run_iteration(&mut plane).unwrap();
        assert!(conn_out(&plane, key).is_empty(), "checkpoint CELL local WAIT replied before LOG");
        assert_eq!(plane.shared.node.checkpoint_in_progress.get(), 1);

        complete_live_checkpoint_publish(&mut cell_loop, &mut plane, &state);

        assert_eq!(conn_out(&plane, key), b"+OK\r\n");
        assert_eq!(plane.shared.checkpoint_publish_gate.pending(), 0);
        assert_eq!(cell_loop.pool().reconcile(), Ok(()));
    }

    #[test]
    fn checkpoint_seed_after_recovered_manifest_uses_successor_id() {
        let state = Rc::new(RefCell::new(DriverState::default()));
        let mut cell_loop = test_loop(Rc::clone(&state), 8);
        let mut plane = test_plane();
        plane.install_log_writer(log_writer());
        plane.seed_checkpoint_next_id_after(CheckpointId::new(9).unwrap());
        plane.install_checkpoint_publisher(checkpoint_publisher());
        let key = insert_test_conn(&plane, 99);
        plane.shared.with_conn(key, |conn| conn.send_inflight = true);
        queue_recv(&state, key, &[b"INF.CKPT", b"WAIT"]);

        cell_loop.run_iteration(&mut plane).unwrap();
        cell_loop.run_iteration(&mut plane).unwrap();
        let (buf, token, _len) = one_log_write(&state);
        state
            .borrow_mut()
            .completions
            .push_back(Completion { token, result: CompletionResult::FileWritten { buf } });
        cell_loop.run_iteration(&mut plane).unwrap();
        cell_loop.run_iteration(&mut plane).unwrap();
        let sync_token = one_log_sync(&state);
        state
            .borrow_mut()
            .completions
            .push_back(Completion { token: sync_token, result: CompletionResult::FileDone });
        cell_loop.run_iteration(&mut plane).unwrap();

        let op = submit_one_checkpoint_op(&mut cell_loop, &mut plane, &state);
        match op {
            IoOp::FileOpen { dir, name, mode, token } => {
                assert_eq!(dir, CHECKPOINT_DIR_FD);
                assert_eq!(name, "ckpt-000010.ick.tmp");
                assert_eq!(mode, FileOpenMode::ReadWriteCreateTruncate);
                assert_eq!(token, checkpoint_token());
            }
            other => panic!("expected checkpoint image open op, got {other:?}"),
        }
        assert!(conn_out(&plane, key).is_empty(), "WAIT replied before checkpoint publish");
        assert_eq!(cell_loop.pool().reconcile(), Ok(()));
    }

    #[test]
    fn checkpoint_wait_fail_stops_without_reply_when_manifest_dir_sync_fails() {
        let state = Rc::new(RefCell::new(DriverState::default()));
        let mut cell_loop = test_loop(Rc::clone(&state), 8);
        let mut plane = test_plane();
        plane.install_log_writer(log_writer());
        plane.install_checkpoint_publisher(checkpoint_publisher());
        let key = insert_test_conn(&plane, 99);
        plane.shared.with_conn(key, |conn| conn.send_inflight = true);
        queue_recv(&state, key, &[b"INF.CKPT", b"WAIT"]);

        cell_loop.run_iteration(&mut plane).unwrap();
        cell_loop.run_iteration(&mut plane).unwrap();
        let (buf, token, _len) = one_log_write(&state);
        state
            .borrow_mut()
            .completions
            .push_back(Completion { token, result: CompletionResult::FileWritten { buf } });
        cell_loop.run_iteration(&mut plane).unwrap();
        cell_loop.run_iteration(&mut plane).unwrap();
        let sync_token = one_log_sync(&state);
        state
            .borrow_mut()
            .completions
            .push_back(Completion { token: sync_token, result: CompletionResult::FileDone });
        cell_loop.run_iteration(&mut plane).unwrap();

        let mut manifest_renamed = false;
        for _ in 0..16 {
            let op = submit_one_checkpoint_op(&mut cell_loop, &mut plane, &state);
            if manifest_renamed {
                let IoOp::FileSync { fd, mode, token } = op else {
                    panic!("expected manifest directory fsync after rename");
                };
                assert_eq!(fd, CHECKPOINT_DIR_FD);
                assert_eq!(mode, FileSyncMode::Full);
                assert_eq!(token, checkpoint_token());
                state.borrow_mut().completions.push_back(Completion {
                    token,
                    result: CompletionResult::Error { errno: TEST_EIO, buf: None },
                });
                break;
            }
            if let IoOp::FileRename { new_name, .. } = &op
                && new_name == inf_log::RECOVERY_MANIFEST_FILE
            {
                manifest_renamed = true;
            }
            let completion = completion_for_checkpoint_op(&op);
            state.borrow_mut().completions.push_back(completion);
            cell_loop.run_iteration(&mut plane).unwrap();
        }

        assert!(manifest_renamed, "test must reach the MANIFEST rename");
        let result = catch_unwind(AssertUnwindSafe(|| cell_loop.run_iteration(&mut plane)));

        assert!(result.is_err(), "manifest directory fsync failure must fail-stop");
        assert!(conn_out(&plane, key).is_empty(), "WAIT replied before durable MANIFEST");
        assert_eq!(cell_loop.pool().reconcile(), Ok(()));
    }

    #[test]
    fn bgsave_accepts_live_checkpoint_request_without_waiting_for_manifest() {
        let state = Rc::new(RefCell::new(DriverState::default()));
        let mut cell_loop = test_loop(Rc::clone(&state), 8);
        let mut plane = test_plane();
        plane.install_log_writer(log_writer());
        plane.install_checkpoint_publisher(checkpoint_publisher());
        let key = insert_test_conn(&plane, 99);
        plane.shared.with_conn(key, |conn| conn.send_inflight = true);
        queue_recv(&state, key, &[b"BGSAVE"]);

        cell_loop.run_iteration(&mut plane).unwrap();

        assert_eq!(conn_out(&plane, key), b"+Background saving started\r\n");
        assert_eq!(plane.shared.node.checkpoint_in_progress.get(), 1);
        assert_eq!(plane.shared.checkpoint_publish_gate.pending(), 0);
    }

    #[test]
    fn bgsave_lastsave_smoke_reports_completed_checkpoint_timestamp() {
        let state = Rc::new(RefCell::new(DriverState::default()));
        let clock = Rc::new(VirtualClock::new(Nanos::from_secs(42)));
        let mut cell_loop = test_loop_with_clock(Rc::clone(&state), 8, Rc::clone(&clock));
        let mut plane = test_plane();
        plane.install_log_writer(log_writer());
        plane.install_checkpoint_publisher(checkpoint_publisher());
        let key = insert_test_conn(&plane, 99);
        plane.shared.with_conn(key, |conn| conn.send_inflight = true);
        queue_recv_many(&state, key, &[&[&b"BGSAVE"[..]], &[&b"LASTSAVE"[..]]]);

        cell_loop.run_iteration(&mut plane).unwrap();

        assert_eq!(conn_out(&plane, key), b"+Background saving started\r\n:0\r\n");
        assert_eq!(plane.shared.node.checkpoint_in_progress.get(), 1);
        assert_eq!(plane.shared.node.last_checkpoint_unix_ms.get(), 0);
        clear_conn_out(&plane, key);

        complete_live_checkpoint_publish(&mut cell_loop, &mut plane, &state);
        assert_eq!(plane.shared.node.last_checkpoint_unix_ms.get(), 42_000);

        queue_recv(&state, key, &[b"LASTSAVE"]);
        cell_loop.run_iteration(&mut plane).unwrap();

        assert_eq!(conn_out(&plane, key), b":42\r\n");
        assert_eq!(cell_loop.pool().reconcile(), Ok(()));
    }

    #[test]
    fn seal_log_queues_staged_frame_and_completes_sync() {
        let state = Rc::new(RefCell::new(DriverState::default()));
        let mut cell_loop = test_loop(Rc::clone(&state), 4);
        let mut plane = test_plane();
        plane.install_log_writer(log_writer());
        let _assignment = stage_delete_always(&plane, b"k");

        cell_loop.run_iteration(&mut plane).unwrap();
        assert_eq!(plane.shared.durability.borrow().log_staging_bytes(), 0);
        assert!(plane.shared.log_writer.borrow().as_ref().unwrap().in_flight());

        cell_loop.run_iteration(&mut plane).unwrap();
        let (buf, token, len) = one_log_write(&state);
        assert!(len > 0);
        state
            .borrow_mut()
            .completions
            .push_back(Completion { token, result: CompletionResult::FileWritten { buf } });

        cell_loop.run_iteration(&mut plane).unwrap();

        assert!(plane.shared.log_writer.borrow().as_ref().unwrap().in_flight());
        cell_loop.run_iteration(&mut plane).unwrap();
        let sync_token = one_log_sync(&state);
        state
            .borrow_mut()
            .completions
            .push_back(Completion { token: sync_token, result: CompletionResult::FileDone });
        cell_loop.run_iteration(&mut plane).unwrap();

        assert!(!plane.shared.log_writer.borrow().as_ref().unwrap().in_flight());
        assert_eq!(cell_loop.pool().reconcile(), Ok(()));
    }

    #[test]
    fn seal_log_rotates_through_prepared_maintenance_segment() {
        let state = Rc::new(RefCell::new(DriverState::default()));
        let mut cell_loop = test_loop(Rc::clone(&state), 4);
        let mut plane = test_plane();
        plane.install_log_writer(rotating_log_writer());
        plane.install_log_segment_maintenance(log_segment_maintenance());

        for _ in 0..4 {
            let op = submit_one_log_maintenance_op(&mut cell_loop, &mut plane, &state);
            complete_one_log_maintenance_op(&state, op);
        }
        state.borrow_mut().pushed.clear();
        cell_loop.run_iteration(&mut plane).unwrap();
        assert!(state.borrow().pushed.iter().all(|op| {
            !matches!(
                op,
                IoOp::FileOpen { token, .. }
                    | IoOp::FilePreallocate { token, .. }
                    | IoOp::FileSync { token, .. } if *token == log_maintenance_token()
            )
        }));

        stage_delete(&plane, b"rotate-key-0123456789-0123456789-0123456789");
        let pending_len =
            plane.shared.durability.borrow().pending_frame_len_bytes().expect("staged frame");
        assert!(
            pending_len > 48 && pending_len <= 96,
            "test frame length {pending_len} must force rotation and fit the segment config"
        );

        let op = submit_one_log_writer_op(&mut cell_loop, &mut plane, &state);
        match op {
            IoOp::FileTruncate { fd, len_bytes, token } => {
                assert_eq!(fd, ACTIVE_LOG_FD);
                assert_eq!(len_bytes, 80);
                assert_eq!(token, log_token());
            }
            other => panic!("unexpected pre-rotate op {other:?}"),
        }
        assert!(
            plane.shared.durability.borrow().pending_frame_len_bytes().is_some(),
            "rotation truncate must not drain staging"
        );
        state
            .borrow_mut()
            .completions
            .push_back(Completion { token: log_token(), result: CompletionResult::FileDone });

        let op = submit_one_log_writer_op(&mut cell_loop, &mut plane, &state);
        match op {
            IoOp::FileSync { fd, mode, token } => {
                assert_eq!(fd, ACTIVE_LOG_FD);
                assert_eq!(mode, FileSyncMode::DataOnly);
                assert_eq!(token, log_token());
            }
            other => panic!("unexpected pre-rotate op {other:?}"),
        }
        assert!(
            plane.shared.durability.borrow().pending_frame_len_bytes().is_some(),
            "rotation sync must not drain staging"
        );
        state
            .borrow_mut()
            .completions
            .push_back(Completion { token: log_token(), result: CompletionResult::FileDone });

        let op = submit_one_log_writer_op(&mut cell_loop, &mut plane, &state);
        let (buf, len) = {
            match op {
                IoOp::FileWriteAt { fd, offset_bytes, buf, len, token } => {
                    assert_eq!(fd, PREPARED_LOG_FD);
                    assert_eq!(offset_bytes, 0);
                    assert_eq!(token, log_token());
                    (buf, len)
                }
                other => panic!("expected rotated log write, got {other:?}"),
            }
        };
        assert_eq!(len, pending_len);
        state.borrow_mut().completions.push_back(Completion {
            token: log_token(),
            result: CompletionResult::FileWritten { buf },
        });
        cell_loop.run_iteration(&mut plane).unwrap();
        assert_eq!(cell_loop.pool().reconcile(), Ok(()));
    }

    #[test]
    fn log_sync_completion_advances_durability_watermark() {
        let state = Rc::new(RefCell::new(DriverState::default()));
        let mut cell_loop = test_loop(Rc::clone(&state), 4);
        let mut waiter_executor = CellExecutor::new(1);
        let mut plane = test_plane();
        plane.install_log_writer(log_writer());
        let _assignment = stage_delete_always(&plane, b"k");

        cell_loop.run_iteration(&mut plane).unwrap();
        cell_loop.run_iteration(&mut plane).unwrap();
        let (buf, token, len) = one_log_write(&state);
        let frame_end = Lsn::new(0, len);
        let watermark_key = log_watermark_key(frame_end);
        let released = Rc::new(Cell::new(false));
        let waiter = plane.shared.durability_gate.waiter(watermark_key);
        let released_for_waiter = Rc::clone(&released);
        let outcome = waiter_executor.poll_immediate(async move {
            waiter.await;
            released_for_waiter.set(true);
        });

        assert!(matches!(outcome, PollImmediate::Suspended(_)));
        assert_eq!(plane.durability_watermark(), 0);
        state
            .borrow_mut()
            .completions
            .push_back(Completion { token, result: CompletionResult::FileWritten { buf } });
        cell_loop.run_iteration(&mut plane).unwrap();
        assert_eq!(plane.durability_watermark(), 0);
        assert_eq!(waiter_executor.run_ready(8), 0);
        assert!(!released.get());

        cell_loop.run_iteration(&mut plane).unwrap();
        let sync_token = one_log_sync(&state);
        state
            .borrow_mut()
            .completions
            .push_back(Completion { token: sync_token, result: CompletionResult::FileDone });
        cell_loop.run_iteration(&mut plane).unwrap();

        assert_eq!(plane.durability_watermark(), watermark_key);
        assert_eq!(waiter_executor.run_ready(8), 1);
        assert!(released.get());
        assert_eq!(waiter_executor.live_tasks(), 0);
        assert_eq!(cell_loop.pool().reconcile(), Ok(()));
    }

    #[test]
    fn maintain_flushes_persistence_log_gauges() {
        let state = Rc::new(RefCell::new(DriverState::default()));
        let mut cell_loop = test_loop(Rc::clone(&state), 4);
        let mut plane = test_plane();
        plane.install_log_writer(log_writer());
        let node = Rc::clone(&plane.shared.node);

        cell_loop.run_iteration(&mut plane).unwrap();
        assert_eq!(node.log_writer_installed.get(), 1);
        assert_eq!(node.log_active_segment.get(), 0);
        assert_eq!(node.log_active_offset_bytes.get(), 0);
        assert_eq!(node.last_durable_lsn.get(), 0);
        assert_eq!(node.watermark_lag_lsn.get(), 0);

        let _assignment = stage_delete_always(&plane, b"k");
        cell_loop.run_iteration(&mut plane).unwrap();
        cell_loop.run_iteration(&mut plane).unwrap();
        let (buf, token, len) = one_log_write(&state);
        state
            .borrow_mut()
            .completions
            .push_back(Completion { token, result: CompletionResult::FileWritten { buf } });
        cell_loop.run_iteration(&mut plane).unwrap();
        cell_loop.run_iteration(&mut plane).unwrap();
        let sync_token = one_log_sync(&state);
        state
            .borrow_mut()
            .completions
            .push_back(Completion { token: sync_token, result: CompletionResult::FileDone });
        cell_loop.run_iteration(&mut plane).unwrap();
        cell_loop.run_iteration(&mut plane).unwrap();

        let durable = log_watermark_key(Lsn::new(0, len));
        assert_eq!(node.pending_log_bytes.get(), 0);
        assert_eq!(node.last_durable_lsn.get(), durable);
        assert_eq!(node.watermark_lag_lsn.get(), 0);
        assert_eq!(node.log_writer_installed.get(), 1);
        assert_eq!(node.log_active_segment.get(), 0);
        assert_eq!(node.log_active_offset_bytes.get(), u64::from(len));
        assert_eq!(node.log_pending_unsynced.get(), 0);
        assert_eq!(cell_loop.pool().reconcile(), Ok(()));
    }

    #[test]
    fn always_reply_waits_for_fsync_watermark() {
        let state = Rc::new(RefCell::new(DriverState::default()));
        let mut cell_loop = test_loop(Rc::clone(&state), 6);
        let mut plane = test_plane();
        plane.install_log_writer(log_writer());
        plane.enable_default_always_for_test();
        let key = insert_test_conn(&plane, 99);
        queue_recv(&state, key, &[b"SET", b"k", b"v"]);

        cell_loop.run_iteration(&mut plane).unwrap();
        assert!(conn_out(&plane, key).is_empty(), "reply escaped before LOG assignment");

        cell_loop.run_iteration(&mut plane).unwrap();
        let (buf, token, len) = one_log_write(&state);
        let watermark_key = log_watermark_key(Lsn::new(0, len));
        assert!(conn_out(&plane, key).is_empty(), "reply escaped before file write completion");
        assert_eq!(plane.durability_watermark(), 0);

        state
            .borrow_mut()
            .completions
            .push_back(Completion { token, result: CompletionResult::FileWritten { buf } });
        cell_loop.run_iteration(&mut plane).unwrap();
        assert!(conn_out(&plane, key).is_empty(), "reply escaped before fdatasync submission");
        assert_eq!(plane.durability_watermark(), 0);

        cell_loop.run_iteration(&mut plane).unwrap();
        let sync_token = one_log_sync(&state);
        assert!(conn_out(&plane, key).is_empty(), "reply escaped before fdatasync completion");
        assert_eq!(plane.durability_watermark(), 0);

        plane.shared.with_conn(key, |conn| conn.send_inflight = true);
        state
            .borrow_mut()
            .completions
            .push_back(Completion { token: sync_token, result: CompletionResult::FileDone });
        cell_loop.run_iteration(&mut plane).unwrap();

        assert_eq!(plane.durability_watermark(), watermark_key);
        assert_eq!(conn_out(&plane, key), b"+OK\r\n");
        assert_eq!(plane.shared.durability_gate.waiting(), 0);
        assert_eq!(cell_loop.pool().reconcile(), Ok(()));
    }

    #[test]
    fn everysec_reply_does_not_wait_for_fsync_and_timer_syncs_later() {
        let state = Rc::new(RefCell::new(DriverState::default()));
        let clock = Rc::new(VirtualClock::new(Nanos::ZERO));
        let mut cell_loop = test_loop_with_clock(Rc::clone(&state), 6, Rc::clone(&clock));
        let mut plane = test_plane();
        plane.install_log_writer(log_writer());
        plane.enable_default_everysec_for_test();
        let key = insert_test_conn(&plane, 99);
        plane.shared.with_conn(key, |conn| conn.send_inflight = true);
        queue_recv(&state, key, &[b"SET", b"k", b"v"]);

        cell_loop.run_iteration(&mut plane).unwrap();
        assert_eq!(conn_out(&plane, key), b"+OK\r\n");
        assert_eq!(plane.durability_watermark(), 0);
        assert!(plane.everysec_timer_armed);
        assert_eq!(log_sync_count(&state), 0);

        cell_loop.run_iteration(&mut plane).unwrap();
        let (buf, token, len) = one_log_write(&state);
        let watermark_key = log_watermark_key(Lsn::new(0, len));
        state
            .borrow_mut()
            .completions
            .push_back(Completion { token, result: CompletionResult::FileWritten { buf } });
        cell_loop.run_iteration(&mut plane).unwrap();
        assert_eq!(plane.durability_watermark(), 0);
        assert!(plane.shared.log_writer.borrow().as_ref().unwrap().has_pending_unsynced());
        assert_eq!(conn_out(&plane, key), b"+OK\r\n");

        state.borrow_mut().pushed.clear();
        clock.advance(Nanos::from_millis(999));
        cell_loop.run_iteration(&mut plane).unwrap();
        cell_loop.run_iteration(&mut plane).unwrap();
        assert_eq!(log_sync_count(&state), 0);
        assert_eq!(plane.durability_watermark(), 0);

        clock.advance(Nanos::from_millis(1));
        cell_loop.run_iteration(&mut plane).unwrap();
        assert!(!plane.everysec_timer_armed);
        assert_eq!(log_sync_count(&state), 0, "timer queues sync for the next submit");
        cell_loop.run_iteration(&mut plane).unwrap();
        let sync_token = one_log_sync(&state);
        assert_eq!(plane.durability_watermark(), 0);

        state
            .borrow_mut()
            .completions
            .push_back(Completion { token: sync_token, result: CompletionResult::FileDone });
        cell_loop.run_iteration(&mut plane).unwrap();

        assert_eq!(plane.durability_watermark(), watermark_key);
        assert_eq!(conn_out(&plane, key), b"+OK\r\n");
        assert_eq!(cell_loop.pool().reconcile(), Ok(()));
    }

    #[test]
    fn public_always_namespace_reply_waits_for_fsync_watermark() {
        let state = Rc::new(RefCell::new(DriverState::default()));
        let mut cell_loop = test_loop(Rc::clone(&state), 6);
        let mut plane = test_plane();
        plane.install_log_writer(log_writer());
        let key = insert_test_conn(&plane, 99);
        plane.shared.with_conn(key, |conn| conn.send_inflight = true);

        queue_recv(
            &state,
            key,
            &[b"INF.NS", b"CREATE", b"ledger", b"MODE", b"durable", b"FSYNC", b"always"],
        );
        cell_loop.run_iteration(&mut plane).unwrap();
        assert_eq!(conn_out(&plane, key), b"+OK\r\n");
        clear_conn_out(&plane, key);

        queue_recv(&state, key, &[b"INF.NS", b"USE", b"ledger"]);
        cell_loop.run_iteration(&mut plane).unwrap();
        assert_eq!(conn_out(&plane, key), b"+OK\r\n");
        clear_conn_out(&plane, key);

        queue_recv(&state, key, &[b"SET", b"k", b"v"]);
        cell_loop.run_iteration(&mut plane).unwrap();
        assert!(conn_out(&plane, key).is_empty(), "reply escaped before LOG assignment");

        cell_loop.run_iteration(&mut plane).unwrap();
        let (buf, token, len) = one_log_write(&state);
        let watermark_key = log_watermark_key(Lsn::new(0, len));
        assert!(conn_out(&plane, key).is_empty(), "reply escaped before file write completion");

        state
            .borrow_mut()
            .completions
            .push_back(Completion { token, result: CompletionResult::FileWritten { buf } });
        cell_loop.run_iteration(&mut plane).unwrap();
        assert!(conn_out(&plane, key).is_empty(), "reply escaped before fdatasync submission");

        cell_loop.run_iteration(&mut plane).unwrap();
        let sync_token = one_log_sync(&state);
        state
            .borrow_mut()
            .completions
            .push_back(Completion { token: sync_token, result: CompletionResult::FileDone });
        cell_loop.run_iteration(&mut plane).unwrap();

        assert_eq!(plane.durability_watermark(), watermark_key);
        assert_eq!(conn_out(&plane, key), b"+OK\r\n");
        assert_eq!(plane.shared.durability_gate.waiting(), 0);
        assert_eq!(cell_loop.pool().reconcile(), Ok(()));
    }

    #[test]
    fn public_everysec_namespace_acks_before_timer_fsync() {
        let state = Rc::new(RefCell::new(DriverState::default()));
        let clock = Rc::new(VirtualClock::new(Nanos::ZERO));
        let mut cell_loop = test_loop_with_clock(Rc::clone(&state), 6, Rc::clone(&clock));
        let mut plane = test_plane();
        plane.install_log_writer(log_writer());
        let key = insert_test_conn(&plane, 99);
        plane.shared.with_conn(key, |conn| conn.send_inflight = true);

        queue_recv(
            &state,
            key,
            &[b"INF.NS", b"CREATE", b"sessions", b"MODE", b"durable", b"FSYNC", b"everysec"],
        );
        cell_loop.run_iteration(&mut plane).unwrap();
        assert_eq!(conn_out(&plane, key), b"+OK\r\n");
        clear_conn_out(&plane, key);

        queue_recv(&state, key, &[b"INF.NS", b"USE", b"sessions"]);
        cell_loop.run_iteration(&mut plane).unwrap();
        assert_eq!(conn_out(&plane, key), b"+OK\r\n");
        clear_conn_out(&plane, key);

        queue_recv(&state, key, &[b"SET", b"k", b"v"]);
        cell_loop.run_iteration(&mut plane).unwrap();
        assert_eq!(conn_out(&plane, key), b"+OK\r\n");
        assert_eq!(plane.durability_watermark(), 0);
        assert!(plane.everysec_timer_armed);

        cell_loop.run_iteration(&mut plane).unwrap();
        let (buf, token, len) = one_log_write(&state);
        let watermark_key = log_watermark_key(Lsn::new(0, len));
        state
            .borrow_mut()
            .completions
            .push_back(Completion { token, result: CompletionResult::FileWritten { buf } });
        cell_loop.run_iteration(&mut plane).unwrap();
        assert_eq!(plane.durability_watermark(), 0);
        assert!(plane.shared.log_writer.borrow().as_ref().unwrap().has_pending_unsynced());

        state.borrow_mut().pushed.clear();
        clock.advance(Nanos::from_secs(1));
        cell_loop.run_iteration(&mut plane).unwrap();
        cell_loop.run_iteration(&mut plane).unwrap();
        let sync_token = one_log_sync(&state);
        state
            .borrow_mut()
            .completions
            .push_back(Completion { token: sync_token, result: CompletionResult::FileDone });
        cell_loop.run_iteration(&mut plane).unwrap();

        assert_eq!(plane.durability_watermark(), watermark_key);
        assert_eq!(conn_out(&plane, key), b"+OK\r\n");
        assert_eq!(cell_loop.pool().reconcile(), Ok(()));
    }

    #[test]
    fn public_mixed_policy_pipeline_uses_one_synced_frame() {
        let state = Rc::new(RefCell::new(DriverState::default()));
        let clock = Rc::new(VirtualClock::new(Nanos::ZERO));
        let mut cell_loop = test_loop_with_clock(Rc::clone(&state), 6, Rc::clone(&clock));
        let mut plane = test_plane();
        plane.install_log_writer(log_writer());
        let key = insert_test_conn(&plane, 99);
        plane.shared.with_conn(key, |conn| conn.send_inflight = true);

        let setup: [&[&[u8]]; 3] = [
            &[&b"INF.NS"[..], &b"CREATE"[..], &b"cache"[..]],
            &[
                &b"INF.NS"[..],
                &b"CREATE"[..],
                &b"sessions"[..],
                &b"MODE"[..],
                &b"durable"[..],
                &b"FSYNC"[..],
                &b"everysec"[..],
            ],
            &[
                &b"INF.NS"[..],
                &b"CREATE"[..],
                &b"ledger"[..],
                &b"MODE"[..],
                &b"durable"[..],
                &b"FSYNC"[..],
                &b"always"[..],
            ],
        ];
        for command in setup {
            queue_recv(&state, key, command);
            cell_loop.run_iteration(&mut plane).unwrap();
            assert_eq!(conn_out(&plane, key), b"+OK\r\n");
            clear_conn_out(&plane, key);
        }

        state.borrow_mut().pushed.clear();
        queue_recv_many(
            &state,
            key,
            &[
                &[&b"INF.NS"[..], &b"USE"[..], &b"sessions"[..]],
                &[&b"SET"[..], &b"e"[..], &b"v"[..]],
                &[&b"INF.NS"[..], &b"USE"[..], &b"cache"[..]],
                &[&b"SET"[..], &b"m"[..], &b"v"[..]],
                &[&b"INF.NS"[..], &b"USE"[..], &b"ledger"[..]],
                &[&b"SET"[..], &b"a"[..], &b"v"[..]],
            ],
        );

        cell_loop.run_iteration(&mut plane).unwrap();
        assert_eq!(conn_out(&plane, key), b"+OK\r\n+OK\r\n+OK\r\n+OK\r\n+OK\r\n");
        assert_eq!(plane.durability_watermark(), 0);
        assert!(!plane.everysec_timer_armed);
        assert_eq!(plane.shared.durability.borrow().log_staging_bytes(), 0);
        assert!(plane.shared.log_writer.borrow().as_ref().unwrap().in_flight());
        assert_eq!(plane.shared.durability_assignment_tickets.borrow().len(), 0);

        cell_loop.run_iteration(&mut plane).unwrap();
        let (buf, token, len) = one_log_write(&state);
        let frame = decode_batch_frame(&cell_loop.pool().bytes(buf)[..len as usize]).unwrap();
        assert_eq!(frame.record_count(), 2);
        assert_eq!(frame.frame_len(), len);
        let watermark_key = log_watermark_key(Lsn::new(0, len));
        assert_eq!(conn_out(&plane, key), b"+OK\r\n+OK\r\n+OK\r\n+OK\r\n+OK\r\n");

        state
            .borrow_mut()
            .completions
            .push_back(Completion { token, result: CompletionResult::FileWritten { buf } });
        cell_loop.run_iteration(&mut plane).unwrap();
        assert_eq!(plane.durability_watermark(), 0);
        assert!(!plane.shared.log_writer.borrow().as_ref().unwrap().has_pending_unsynced());

        state.borrow_mut().pushed.clear();
        cell_loop.run_iteration(&mut plane).unwrap();
        let sync_token = one_log_sync(&state);
        assert_eq!(plane.durability_watermark(), 0);
        assert_eq!(conn_out(&plane, key), b"+OK\r\n+OK\r\n+OK\r\n+OK\r\n+OK\r\n");

        state
            .borrow_mut()
            .completions
            .push_back(Completion { token: sync_token, result: CompletionResult::FileDone });
        cell_loop.run_iteration(&mut plane).unwrap();
        assert_eq!(plane.durability_watermark(), watermark_key);
        assert_eq!(conn_out(&plane, key), b"+OK\r\n+OK\r\n+OK\r\n+OK\r\n+OK\r\n+OK\r\n");
        assert_eq!(plane.shared.durability_gate.waiting(), 0);

        state.borrow_mut().pushed.clear();
        clock.advance(Nanos::from_secs(1));
        cell_loop.run_iteration(&mut plane).unwrap();
        cell_loop.run_iteration(&mut plane).unwrap();
        assert_eq!(log_sync_count(&state), 0);
        assert!(!plane.everysec_timer_armed);
        assert_eq!(cell_loop.pool().reconcile(), Ok(()));
    }

    fn drain_one_origin_reply(fabric: &mut CellFabric) -> Option<Vec<u8>> {
        let mut reply = None;
        fabric.drain(8, |_, op| match op {
            Op::Reply { outcome: Outcome::Bytes(bytes), .. } => {
                reply = Some(bytes.to_vec());
            }
            other => panic!("unexpected origin fabric op {other:?}"),
        });
        reply
    }

    #[test]
    fn remote_always_apply_reply_waits_for_owner_fsync_watermark() {
        let state = Rc::new(RefCell::new(DriverState::default()));
        let mut cell_loop = test_loop(Rc::clone(&state), 6);
        let mut fabrics =
            Mesh::new(2, MeshConfig { ring_capacity: 64, data_credits: 16 }).into_iter();
        let mut origin_fabric = fabrics.next().expect("origin fabric");
        let owner_fabric = fabrics.next().expect("owner fabric");
        let mut owner = test_plane_with_fabric(CellId(1), 2, owner_fabric);
        owner.install_log_writer(log_writer());
        owner.enable_default_always_for_test();

        let token = origin_fabric.next_token();
        let args = ApplyArgs::new(&[&b"SET"[..], b"k", b"v"]).expect("valid apply args");
        let op = Op::Apply { token, slot: SlotRouter::slot_of(b"k"), namespace: 0, cmd: 2, args };
        origin_fabric.send(CellId(1), &op).expect("origin has credit");
        origin_fabric.flush();

        cell_loop.run_iteration(&mut owner).unwrap();
        assert!(
            drain_one_origin_reply(&mut origin_fabric).is_none(),
            "remote reply escaped before owner LOG assignment"
        );

        cell_loop.run_iteration(&mut owner).unwrap();
        let (buf, token, len) = one_log_write(&state);
        let watermark_key = log_watermark_key(Lsn::new(0, len));
        assert!(
            drain_one_origin_reply(&mut origin_fabric).is_none(),
            "remote reply escaped before owner write completion"
        );

        state
            .borrow_mut()
            .completions
            .push_back(Completion { token, result: CompletionResult::FileWritten { buf } });
        cell_loop.run_iteration(&mut owner).unwrap();
        assert!(
            drain_one_origin_reply(&mut origin_fabric).is_none(),
            "remote reply escaped before owner fdatasync submission"
        );

        cell_loop.run_iteration(&mut owner).unwrap();
        let sync_token = one_log_sync(&state);
        assert!(
            drain_one_origin_reply(&mut origin_fabric).is_none(),
            "remote reply escaped before owner fdatasync completion"
        );

        state
            .borrow_mut()
            .completions
            .push_back(Completion { token: sync_token, result: CompletionResult::FileDone });
        cell_loop.run_iteration(&mut owner).unwrap();

        assert_eq!(owner.durability_watermark(), watermark_key);
        assert_eq!(drain_one_origin_reply(&mut origin_fabric), Some(b"+OK\r\n".to_vec()));
        assert_eq!(owner.shared.durability_gate.waiting(), 0);
        assert_eq!(owner.shared.owner_durable_replies.borrow().len(), 0);
        assert!(!owner.shared.owner_durable_pump_active.get());
        assert_eq!(cell_loop.pool().reconcile(), Ok(()));
    }

    #[test]
    fn remote_checkpoint_wait_apply_reply_waits_for_owner_manifest_publish() {
        let state = Rc::new(RefCell::new(DriverState::default()));
        let mut cell_loop = test_loop(Rc::clone(&state), 8);
        let mut fabrics =
            Mesh::new(2, MeshConfig { ring_capacity: 64, data_credits: 16 }).into_iter();
        let mut origin_fabric = fabrics.next().expect("origin fabric");
        let owner_fabric = fabrics.next().expect("owner fabric");
        let mut owner = test_plane_with_fabric(CellId(1), 2, owner_fabric);
        owner.install_log_writer(log_writer());
        owner.install_checkpoint_publisher(checkpoint_publisher());

        let token = origin_fabric.next_token();
        let args = ApplyArgs::new(&[&b"INF.CKPT"[..], b"WAIT"]).expect("valid apply args");
        let op = Op::Apply { token, slot: SlotRouter::slot_of(b""), namespace: 0, cmd: 2, args };
        origin_fabric.send(CellId(1), &op).expect("origin has credit");
        origin_fabric.flush();

        cell_loop.run_iteration(&mut owner).unwrap();
        assert!(
            drain_one_origin_reply(&mut origin_fabric).is_none(),
            "remote checkpoint WAIT replied before owner LOG"
        );
        assert_eq!(owner.shared.node.checkpoint_in_progress.get(), 1);

        complete_live_checkpoint_publish(&mut cell_loop, &mut owner, &state);

        assert_eq!(drain_one_origin_reply(&mut origin_fabric), Some(b"+OK\r\n".to_vec()));
        assert_eq!(owner.shared.checkpoint_publish_gate.pending(), 0);
        assert_eq!(owner.shared.owner_checkpoint_replies.borrow().len(), 0);
        assert!(!owner.shared.owner_checkpoint_pump_active.get());
        assert_eq!(cell_loop.pool().reconcile(), Ok(()));
    }

    #[test]
    fn checkpoint_cell_remote_wait_replies_after_owner_manifest_publish() {
        let origin_state = Rc::new(RefCell::new(DriverState::default()));
        let owner_state = Rc::new(RefCell::new(DriverState::default()));
        let mut origin_loop = test_loop(Rc::clone(&origin_state), 8);
        let mut owner_loop = test_loop(Rc::clone(&owner_state), 8);
        let mut fabrics =
            Mesh::new(2, MeshConfig { ring_capacity: 64, data_credits: 16 }).into_iter();
        let origin_fabric = fabrics.next().expect("origin fabric");
        let owner_fabric = fabrics.next().expect("owner fabric");
        let mut origin = test_plane_with_fabric(CellId(0), 2, origin_fabric);
        let mut owner = test_plane_with_fabric(CellId(1), 2, owner_fabric);
        owner.install_log_writer(log_writer());
        owner.install_checkpoint_publisher(checkpoint_publisher());
        let key = insert_test_conn(&origin, 99);
        origin.shared.with_conn(key, |conn| conn.send_inflight = true);
        queue_recv(&origin_state, key, &[b"INF.CKPT", b"CELL", b"1", b"WAIT"]);

        origin_loop.run_iteration(&mut origin).unwrap();
        assert!(conn_out(&origin, key).is_empty(), "origin replied before owner accepted");

        owner_loop.run_iteration(&mut owner).unwrap();
        assert!(conn_out(&origin, key).is_empty(), "origin replied before owner LOG");
        assert_eq!(owner.shared.node.checkpoint_in_progress.get(), 1);

        complete_live_checkpoint_publish(&mut owner_loop, &mut owner, &owner_state);
        assert!(conn_out(&origin, key).is_empty(), "origin replied before draining fabric reply");

        for _ in 0..4 {
            origin_loop.run_iteration(&mut origin).unwrap();
            if conn_out(&origin, key) == b"+OK\r\n" {
                break;
            }
        }

        assert_eq!(conn_out(&origin, key), b"+OK\r\n");
        assert_eq!(owner.shared.checkpoint_publish_gate.pending(), 0);
        assert_eq!(owner.shared.owner_checkpoint_replies.borrow().len(), 0);
        assert!(!owner.shared.owner_checkpoint_pump_active.get());
        assert_eq!(origin_loop.pool().reconcile(), Ok(()));
        assert_eq!(owner_loop.pool().reconcile(), Ok(()));
    }

    #[test]
    fn seal_log_preserves_staging_when_pool_is_exhausted() {
        let state = Rc::new(RefCell::new(DriverState {
            lease_one_on_submit: true,
            ..DriverState::default()
        }));
        let mut cell_loop = test_loop(Rc::clone(&state), 1);
        let mut plane = test_plane();
        plane.install_log_writer(log_writer());
        let _assignment = stage_delete_always(&plane, b"k");
        let staged_before = plane.shared.durability.borrow().log_staging_bytes();

        cell_loop.run_iteration(&mut plane).unwrap();

        assert_eq!(plane.shared.durability.borrow().log_staging_bytes(), staged_before);
        assert!(state.borrow().pushed.is_empty());
        assert!(!plane.shared.log_writer.borrow().as_ref().unwrap().in_flight());

        state.borrow_mut().release_held_on_submit = true;
        cell_loop.run_iteration(&mut plane).unwrap();
        cell_loop.run_iteration(&mut plane).unwrap();
        let (buf, token, _) = one_log_write(&state);
        state
            .borrow_mut()
            .completions
            .push_back(Completion { token, result: CompletionResult::FileWritten { buf } });
        cell_loop.run_iteration(&mut plane).unwrap();
        cell_loop.run_iteration(&mut plane).unwrap();
        let sync_token = one_log_sync(&state);
        state
            .borrow_mut()
            .completions
            .push_back(Completion { token: sync_token, result: CompletionResult::FileDone });
        cell_loop.run_iteration(&mut plane).unwrap();

        assert_eq!(cell_loop.pool().reconcile(), Ok(()));
    }

    #[test]
    fn log_write_error_returns_buffer_before_fail_stop() {
        let state = Rc::new(RefCell::new(DriverState::default()));
        let mut cell_loop = test_loop(Rc::clone(&state), 4);
        let mut plane = test_plane();
        plane.install_log_writer(log_writer());
        let _assignment = stage_delete_always(&plane, b"k");

        cell_loop.run_iteration(&mut plane).unwrap();
        cell_loop.run_iteration(&mut plane).unwrap();
        let (buf, token, _) = one_log_write(&state);
        state.borrow_mut().completions.push_back(Completion {
            token,
            result: CompletionResult::Error { errno: TEST_EIO, buf: Some(buf) },
        });

        let result = catch_unwind(AssertUnwindSafe(|| cell_loop.run_iteration(&mut plane)));

        assert!(result.is_err(), "write error must fail-stop");
        assert_eq!(cell_loop.pool().reconcile(), Ok(()));
    }

    #[test]
    fn log_fsync_error_fail_stops_after_write_buffer_return() {
        let state = Rc::new(RefCell::new(DriverState::default()));
        let mut cell_loop = test_loop(Rc::clone(&state), 4);
        let mut plane = test_plane();
        plane.install_log_writer(log_writer());
        let _assignment = stage_delete_always(&plane, b"k");

        cell_loop.run_iteration(&mut plane).unwrap();
        cell_loop.run_iteration(&mut plane).unwrap();
        let (buf, token, _) = one_log_write(&state);
        state
            .borrow_mut()
            .completions
            .push_back(Completion { token, result: CompletionResult::FileWritten { buf } });
        cell_loop.run_iteration(&mut plane).unwrap();
        cell_loop.run_iteration(&mut plane).unwrap();
        let sync_token = one_log_sync(&state);
        assert_eq!(cell_loop.pool().reconcile(), Ok(()));
        state.borrow_mut().completions.push_back(Completion {
            token: sync_token,
            result: CompletionResult::Error { errno: TEST_EIO, buf: None },
        });

        let result = catch_unwind(AssertUnwindSafe(|| cell_loop.run_iteration(&mut plane)));

        assert!(result.is_err(), "fsync error must fail-stop");
        assert_eq!(cell_loop.pool().reconcile(), Ok(()));
    }

    #[test]
    fn staged_log_without_writer_fails_fast() {
        let state = Rc::new(RefCell::new(DriverState::default()));
        let mut cell_loop = test_loop(state, 4);
        let mut plane = test_plane();
        stage_delete(&plane, b"k");

        let result = catch_unwind(AssertUnwindSafe(|| cell_loop.run_iteration(&mut plane)));

        assert!(result.is_err(), "staged durable bytes without a writer must not be ignored");
    }
}
