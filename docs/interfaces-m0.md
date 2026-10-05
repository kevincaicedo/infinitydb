# M0 Interface Freeze

Authoritative Rust signatures for the cross-crate seams frozen at M0 exit.
Changing one of these after M0 requires an ADR.
Implementations may add private detail and additional inherent methods, but
the shapes below are the contract that `inf-server`, `inf-sim`, and `inf-bench`
are built against.

Conventions: edition 2024, `#![forbid(unsafe_code)]` everywhere except
`inf-simd`, `inf-alloc`, `inf-fabric` (ring internals), `inf-runtime`
(uring/kqueue FFI). No `dyn` on hot paths; generics stay monomorphized.
Time and randomness are always injected (`inf_foundation::time`, L7).

---

## 1. `inf-foundation` (implemented — the code is the spec)

```rust
pub struct CellId(u16);                    // + as_usize(), Display
pub struct KeySlot(/* private */);         // invariant: 0..16384, checked constructor
pub const SLOT_COUNT: u16 = 16384;

pub mod time {
    pub struct Nanos(pub u64);             // monotonic, ord, arithmetic helpers
    pub trait Clock { fn now(&self) -> Nanos; }
    pub struct StdClock;                   // Instant-based
    pub struct VirtualClock;               // Cell<u64>; set/advance — sim & tests
}
pub mod rng {
    pub trait Entropy { fn next_u64(&mut self) -> u64; }
    pub struct SplitMix64;                 // seeded, deterministic
}
pub fn hash64(data: &[u8], seed: u64) -> u64;      // wyhash-style digest, stable — NOT the key hash (ADR-0094)
pub struct KeyHasher;                              // SipHash-1-3 under a per-data-directory secret: `hash(&self, key)` — the index hash (ADR-0094)
pub fn siphash13(k0: u64, k1: u64, data: &[u8]) -> u64;
pub fn crc16(data: &[u8]) -> u16;                  // XMODEM, Redis Cluster vectors
pub fn hashtag(key: &[u8]) -> &[u8];               // Redis Cluster {tag} rule
pub mod varint { encode_u64 / decode_u64 }
pub struct LogHistogram;                   // record(u64) / percentile(f64) / max / count
pub struct CachePadded<T>(pub T);          // #[repr(align(128))]
pub struct LocalCounter;                   // Cell<u64>: no atomics (L1)
pub mod tripwire { /* frozen counter names */ }
```

## 2. `inf-alloc` (implemented — the code is the spec)

```rust
// Buffer pool (wire buffers, registered with the backend).
// Fixed capacity; buffer addresses are stable for the pool's lifetime
// (io_uring fixed-buffer registration relies on this).
pub struct BufferPool;
pub struct BufferId(/* private u32 */);
pub enum LeaseKind { Recv, Send }
impl BufferPool {
    pub fn new(count: usize, buf_size: usize) -> Self;
    pub fn try_lease(&mut self, kind: LeaseKind) -> Option<BufferId>;
    pub fn release(&mut self, id: BufferId);          // panics on double-release
    pub fn bytes(&self, id: BufferId) -> &[u8];
    pub fn bytes_mut(&mut self, id: BufferId) -> &mut [u8];
    pub fn buf_size(&self) -> usize;  pub fn leased(&self) -> usize;
    pub fn reconcile(&self) -> Result<(), LeaseLeak>; // CONSUMER-leak test hook

    // 2026-06-11 extension (recorded deviation; first live uring run):
    // a third custody state for kernel provided-buffer rings. Staged
    // buffers are neither free nor consumer-leased; reconcile() ignores
    // them (a live provided group legitimately holds custody). The uring
    // driver stages at most HALF the pool — staging everything starved
    // the send path (deadlock by buffer exhaustion, found by the
    // conformance suite on Linux).
    pub fn try_stage(&mut self) -> Option<BufferId>;   // Free → Staged
    pub fn promote_staged(&mut self, id: BufferId);    // Staged → Leased(Recv)
    pub fn unstage(&mut self, id: BufferId);           // Staged → Free
    pub fn staged(&self) -> usize;  pub fn available(&self) -> usize;
}

// ADR-0161 replaces allocating Arena::new with checked, fallible
// preparation. The signature below is the implemented shape; the
// replacement is not built.
// Record arena (M0-S13): size-class slabs over anonymous-mmap chunks.
// Classes: 16..=256 in 8 B steps, then ×1.25 geometric to chunk_size/4;
// larger allocations get dedicated page-rounded mappings (unmap on free).
// ArenaAddr packs {chunk:27, offset:21} = the 48-bit `addr` the index
// slot stores. Bump-within-chunk + intrusive free lists keep untouched
// pages uncommitted. `resize_in_place` covers same-class grow AND shrink.
pub struct Arena;
pub struct ArenaAddr(/* private u48 */);   // to_raw()/from_raw()
pub struct ArenaConfig { pub chunk_size: usize, pub max_resident: Option<usize> }
impl Arena {
    pub fn new(config: ArenaConfig) -> Self;
    pub fn alloc(&mut self, len: usize) -> Option<ArenaAddr>;   // None = budget exhausted
    pub fn free(&mut self, addr: ArenaAddr, len: usize);
    pub fn resize_in_place(&mut self, addr: ArenaAddr, old: usize, new: usize) -> bool;
    pub fn bytes(&self, addr: ArenaAddr, len: usize) -> &[u8];
    pub fn bytes_mut(&mut self, addr: ArenaAddr, len: usize) -> &mut [u8];
    pub fn report(&self) -> ArenaReport;   // live/slack/resident bytes, live_allocs — byte-exact
}
```

## 3. `inf-runtime` — backend driver + executor + loop (implemented core; pending changes marked)

> **Accepted 2026-09-22, implementation open — ADR-0149:** reserved
> executor and gate capacity replaces the executor/gate part of the
> implemented sketch below. Fallible boot construction yields fixed task
> classes; `reserve(class)` returns an exclusive permit whose
> `poll_immediate`/`spawn_local` methods accept a factory only after
> admission. Ready publishes no runnable task; retained waker headers
> still own capacity. `run_ready` returns `ExecutorProgress {
> tasks_polled, slots_reclaimed }`, charging both against the slice
> budget. `live_tasks` alone is not a leak proof. `KeyedGate<K, V,
> Cleanup>` reserves routing and holder capacity before request
> publication; completed values retain their payload ownership.
> `IoGate<Cleanup>` keeps its name with an explicit terminal-cleanup
> policy. Admission: `reserve` answers Granted, Full, Closed, identity
> exhaustion or a foreign class; no refusal owns the future or its
> factory, and a producer reserves before it dequeues work or causes an
> effect. Transitions: a class slot is Free, Reserved, Active or Retired;
> a completed future is dropped once, and its slot returns only when no
> external waker remains (Reserved + Active + Retired ≤ the class's
> slots). Failure: a construction refusal releases partial backing and
> takes part in the all-cells boot barrier, and task-identity exhaustion
> is a typed refusal before any effect, never a wrap (ADR-0149 D1–D3). The
> signatures below are the implemented shape; the replacement above is not
> built.

> **Accepted 2026-09-22, implementation open — ADR-0151:** fixed storage
> and bounded cell maps narrows `KeyedGate` keys to the runtime's sealed,
> exact fixed-width adapter and `WaitList` keys to the foundation's sealed
> key domain. Fabric and I/O gates retain their current key types and
> every identity bit. Admission, terminal cleanup and the 5 ns executor
> gate are unchanged. A key is a sealed, fixed-width exact representation
> (an integer of at most 128 bits, a pair of `u64`s, the unit key or a
> foundation ID with an exact integer form), compared with constant work
> and never through a caller's `Hash` or comparison. A map answers
> `Occupied`, `Vacant` or `Full`, and a vacant slot fixes the key and
> every tree position before the payload is built (ADR-0151 D1, D2).

> **Accepted 2026-09-22, implementation open — ADR-0154:** fixed timer
> ownership and bounded callback delivery replaces TimerWheel/TimerId and
> the raw-key on_timer contract below. Fallible boot construction admits
> fixed TimerSet owner positions and an indexed minimum heap.
> Cancel/replace physically removes the prior entry; owner/arm identities
> are checked and do not wrap. Delivery keeps the position through
> synchronous callback consumption, with at most 64 callbacks per native
> turn and one successor after consumption. LoopCx exposes admitted
> owner/arm operations, not unbounded advance or allowance reset. The
> runtime retains the terminal receipt; a raw route key cannot authorize
> callback delivery to a replacement owner. A full set refuses before
> publication and never allocates or drops a wake; arming an owner during
> its delivery answers Busy; a stale owner, arm or receipt gets a typed
> stale result; and close wins over a successor. A deadline is normalized
> to no earlier than the next millisecond after the current turn, and
> clock-range exhaustion is a typed error before arming (ADR-0154 D1–D3).
> The sketch below is the implemented shape; the replacement above is not
> built. The ten phases, one backend entry and existing gates remain
> unchanged.

> **Accepted 2026-09-22, implementation open — ADR-0147:** bounded accept
> admission and terminal parking replaces native multishot accept with
> bounded batches. It adds `IoOp::AcceptPark { listener: RawFd, token:
> CompletionToken }`, `CompletionResult::AcceptParked`, and the routing
> classes `TokenClass::RefusalSend` / `TokenClass::RefusalClose`; the
> token layout is unchanged. A stale generation never retargets a
> successor listener, and an exhausted generation retires its identity
> instead of wrapping. A native socket stays counted from delivery until a
> reserved connection slot, a fabric handoff or its terminal close takes
> it. Each listener has at most 16 one-shot accept attempts
> (`ACCEPT_ATTEMPTS_PER_LISTENER`) and each cell at most 64 native
> custodies (`NATIVE_ACCEPT_CUSTODIES_PER_CELL`): a cell parks at 48 and
> resumes only after `AcceptParked`, with at most 32 left. An accept error
> parks production too, and neither its retry timer nor an unrelated close
> clears an explicit park (ADR-0147 D1–D3). `AcceptParked` settles both
> original accept and cancellation completions, not just the cancel
> request. The sketch below is the implemented shape; these additions are
> not built.

> (edition-2024 keyword), the Pin-sound `PollImmediate` shape,
> `FabricGate<V>`, `submit_stats()`/`performance_tier`, fallible
> `run_iteration`, `CellPlane::on_timer`.
>
> **Extended at M2-S05 under the freeze discipline (ADR-0013):** `IoOp`
> gains `LogWrite`/`Fdatasync`, `CompletionResult` gains
> `LogWritten`/`Synced`, `TokenClass` gains `LogWrite = 5`/`Fsync = 6`.
> Layouts unchanged; the new surface is documented in `interfaces-m2.md`.
>
> **Extended at M4-S04 under the same discipline:** `IoOp` gains
> `TierRead { fd, offset, buf: StableBytesMut, token }` (positional
> cold-tier read; short reads resubmitted internally — the completion
> means the buffer is FULL), `CompletionResult` gains `TierRead`,
> `TokenClass` gains `TierRead = 10`, and `StableBytesMut` joins
> `StableBytes` as the writable stable-range handoff. Layouts unchanged.
>
> **Amended at M4.5-S34 under the same discipline (ADR-0086 D1):**
> `IoOp::LogWrite`'s `fsync_token: Option<CompletionToken>` became
> `barrier: WriteBarrier { None | WriteThrough | LinkedFsync { fsync_token } }`
> — one frame, one barrier; `WriteThrough` (`RWF_DSYNC` on `O_DIRECT`,
> the FUA class) makes `LogWritten` the frame's durability fact.
> `TokenClass` gains `ZeroFillWrite = 13` (routing-only). Layouts
> unchanged; the surface is documented in `interfaces-m2.md`.
> The first real consumer of the `IoGate` seam; the cold-read path
> freezes at M4 exit after S08 hardens it.
>
> **Amended 2026-09-30 (ADR-0167 D1/D2):**
> `IoOp::TierRead.offset` and `IoOp::LogWrite.offset` are
> `inf_foundation::FileOffset`, not `u64`: a position in
> `0..=FILE_OFFSET_BYTES_MAX` (`i64::MAX − u32::MAX`), so an op's span
> end is at most `i64::MAX` and never the kernel's `−1` current-position
> sentinel. It is built only by the range check `FileOffset::new`
> (refused as `FileOffsetRefused`, carrying the value) or the total
> `from_u32_bytes`; backends read `bytes_after` and `position_after`.
> Token layout unchanged.
>
> **Extended at M4-S08 under the same discipline:** `BackendDriver`
> gains `register_tier_pool(&mut self, pool: &mut AlignedPool)` (default
> no-op — readiness/sim backends serve `TierRead` positionally either
> way). On io_uring a call makes the aligned pool's buffers the ring's
> registered-buffer table, replacing the boot-time recv-pool
> registration (a capability probe with no consumer; io_uring has one
> table); in-range `TierRead` ops then take the fixed-buffer read opcode,
> and registration failure degrades `Capabilities::fixed_buffers` instead
> of failing boot. No shipped path calls it: the server builds its
> cold-read pool (`TierCell::create_ns`) unregistered, so every cold read
> is a plain positional `Read`; one test and two benches register. ADR-0153
> (below) replaces the method; until it is built, no cold read uses a
> fixed buffer. The custody
> vocabulary above it — `ColdReads` / `ColdDone` / `TierFileId` in
> `inf_runtime::cold` — is the cold-read-path freeze content
> (aligned-pool contract, `IoToken` usage, per-file pins, and the
> `inflight_total` concurrency-limit hook S10 consumes).
>
> **Reshaped at M4-S10 (pre-freeze — the cold-read path freezes at M4
> exit, ADR-0055):** `ColdReads::issue` (eager lease + op build) is
> replaced by `enqueue(fd, file, offset, len, ReadClass, now_us) →
> ColdWait` + `drain(push) → issued` — bounded per-class intent FIFOs,
> per-cell device-QD cap, 3:1 foreground:maintain deficit, and
> adjacent/overlapping same-file coalescing with shared-window `ColdDone`
> fan-out (last drop releases the lease). `on_completion` gains the
> injected `now_us` and returns the delivered-waiter count. The driver
> contract is untouched: a merged read is one ordinary `TierRead` whose
> window fits one pool buffer (the `ReadFixed` upgrade applies only to a
> registered pool).
> `KeyedGate` gains `has_waiter` (drain-side stale-intent skip). No
> `IoOp`/`CompletionResult`/`TokenClass` layout change.
>
> **Amended 2026-09-30 (ADR-0167 D3):** `enqueue`
> checks the position, then `len == 0`, then `len > buf_size` (on the
> `usize`), before the queue bound and before any state changes, and
> answers the first fault with the permanent
> `ColdRefused::Unrepresentable(OffsetAboveMax | EmptyWindow |
> WindowAboveMax)`, carrying the refused value where there is one, even
> when the class queue is full; it changes no depth, pin, token or
> counter. `ColdRefused` is exhaustive. `with_config` panics on a pool
> buffer above `DRIVER_OP_BYTES_MAX` (`u32::MAX`). A merged read's span
> end is `bytes_after(len)`, and a union stays within `buf_size`.

> **Accepted 2026-09-22, implementation open — ADR-0152:** bounded
> cold-read result delivery replaces the implemented full `on_completion
> -> delivered_count` fan-out above with `record_completion(token, result,
> now_us) -> ()`, `deliver_ready(&mut ColdDeliveryBudget, now_us) ->
> ColdDeliveryProgress` and `has_ready_delivery() -> bool`. One host-turn
> allowance covers at most 64 logical deliveries, including orphan
> cleanup. The cell's execute prelude specified by A1 below services it;
> pending delivery prevents parking. Device receipt returns QD, while
> file/buffer custody remains through final value drop. Routing and holder
> storage cover deferred delivery; latency includes that delay. A window
> goes Free → Issued (it owns the buffer, its member prefix and every file
> pin) → Ready at the device receipt → one member per budgeted step, and
> back to Free only when no unvisited member or delivered value remains; a
> cancelled member costs a step like an orphan. `cold_reads_inflight`
> keeps counting reads that await the device; pending-delivery members and
> windows and delivery work get their own counters; and `cold_read_p99_us`
> samples at logical delivery, with device-to-delivery delay observed
> apart (ADR-0152 D3, D4). The `on_completion` fan-out above is the
> implemented shape; this replacement is not built.

> **Accepted 2026-09-22, implementation open — ADR-0152 A1:** the optional
> early executor pass precedes `parse_execute`. The A1 correction adds a
> default-no-op `CellPlane::before_execute` hook after FABRIC-IN and
> before both scheduled executor passes, borrowing one native-turn cold
> budget from `LoopCx`.

> **Accepted 2026-09-22, implementation open — ADR-0152 A2:** bounded
> preparation adds a separate host-turn preparation budget to both cold
> drain calls and lends it through LoopCx. It forms complete bounded
> cohorts, keeps unexamined requests queued, reserves identity pairs at
> enqueue and adds typed progress/cancellation notification with one retry
> timer. Both `drain` and `drain_budgeted` receive `&mut
> ColdPreparationBudget` and return `ColdPreparationProgress`;
> `has_ready_preparation() -> bool` reads stored readiness. A turn spends
> at most `COLD_PREPARATION_WORK_PER_TURN` = 64 work units (a head
> inspection one, any other candidate two), so a cohort holds at most 31
> members. The drain reports `Idle`, `CpuPending`, `WaitDevice`,
> `WaitPool` or `WaitIoBudget`, and only `CpuPending` asks for another
> immediate turn. An exhausted identity pair is the permanent
> `ColdRefused::IdentityExhausted`, answered before any queue, gate or pin
> is published. A cancelled `ColdWait` notifies the cold owner after the
> gate restores custody (ADR-0152 A2).

> **Accepted 2026-09-22, implementation open — ADR-0153:** cold-pool
> construction and native registration establishes a fallible chunked pool
> and owned driver binding/release lifecycle. Registration remains on the
> issuer, with a ring-owned sparse table and bounded native
> batches/terminal tags. It replaces the borrowed register_tier_pool
> method with owned ColdPoolBind/ColdPoolClose operations and
> ColdPoolReady/ColdPoolReleased receipts. A binding reserves a
> non-wrapping pool identity and its native resource IDs before any
> effect, and an old, duplicate or foreign receipt never releases current
> backing. No cold read is admitted before the binding answers
> `ReadyFixed` or `ReadyPlain`, and a serving turn makes at most one
> registration call of at most 16 entries and 64 KiB. On failure or close
> the installed prefix is kept, reads and held values are joined, and the
> registrations are removed by the same bounded protocol (ADR-0153 D2–D4).

```rust
pub struct CompletionToken(u64);           // {class:8, slot:24, gen:32}
pub enum TokenClass { Accept, Recv, Send, Close, Wake }
impl CompletionToken {
    pub fn new(class: TokenClass, slot: u32, generation: u32) -> Self;  // slot < 2^24
    pub fn class(self) -> TokenClass;  pub fn slot(self) -> u32;
    pub fn generation(self) -> u32;    // `gen` is a reserved keyword (edition 2024)
    pub fn as_u64(self) -> u64;  pub fn from_u64(raw: u64) -> Option<Self>;
}

pub enum IoOp {
    /// Implemented accept: one arm yields Accepted until disarmed/error.
    /// Accepted ADR-0147's bounded batches and AcceptPark are not yet built.
    /// ADR-0118: an accept failure is classified by ONE table on
    /// every backend — `classify_accept_errno(errno) -> AcceptFailure::
    /// {Transient, Exhausted, Broken}`. Transient ⇒ nothing delivered, the arm
    /// stays up; Exhausted/Broken ⇒ one `Error` on the listener token and the
    /// arm is PARKED (never re-armed into the same failure). A parked arm
    /// resumes on a later `AcceptArm` (idempotent while armed) and, for
    /// Exhausted, on any `Closed` fd of the same driver. io_uring captures
    /// `RLIMIT_NOFILE` when the SQE is prepared: a raised limit lands at the
    /// next park/resume.
    AcceptArm { listener: RawFd, token: CompletionToken },
    /// Provided-buffer recv: the DRIVER leases recv buffers from the pool and
    /// delivers them in completions; the consumer must `release` each one.
    /// Multishot where the backend supports it; re-armed internally otherwise.
    RecvArm { fd: RawFd, token: CompletionToken },
    /// Backpressure seam (fabric credits exhausted → stop reading this conn).
    RecvDisarm { fd: RawFd },
    /// Completes only when all `len` bytes are written, or terminal error.
    /// Buffer was leased by the caller; ownership returns in the completion.
    Send { fd: RawFd, buf: BufferId, len: u32, token: CompletionToken },
    Close { fd: RawFd, token: CompletionToken },
}

pub struct Completion { pub token: CompletionToken, pub result: CompletionResult }
pub enum CompletionResult {
    Accepted { fd: RawFd },
    Recv { buf: BufferId, len: u32 },      // len == 0 ⇒ peer closed (EOF)
    RecvDropped,                            // pool dry; recv paused, re-arm needed
    Sent { buf: BufferId },
    Closed,
    Error { errno: i32, buf: Option<BufferId> }, // any held buffer ALWAYS returns
}

pub enum Wait { Poll, Park { timeout: Option<Duration> } }

pub struct Capabilities {                  // boot-logged feature probe
    pub backend: &'static str,
    pub multishot_accept: bool, pub multishot_recv: bool,
    pub provided_buffers: bool, pub fixed_buffers: bool,
    pub single_issuer: bool,    pub defer_taskrun: bool,
    /// kqueue dev tier is false — gate tooling rejects it mechanically.
    pub performance_tier: bool,
}
pub struct SubmitStats { pub syscalls: u64, pub sqes: u64, pub cqes: u64 }

pub trait BackendDriver {
    fn push(&mut self, op: IoOp);                       // queue; no syscall
    /// ONE backend entry for all queued submissions + reap (L3).
    fn submit_and_reap(
        &mut self, pool: &mut BufferPool, wait: Wait, out: &mut Vec<Completion>,
    ) -> io::Result<usize>;
    fn register_pool(&mut self, pool: &mut BufferPool) -> io::Result<()>;
    fn capabilities(&self) -> Capabilities;
    fn submit_stats(&self) -> SubmitStats;              // feeds sqes_per_submit/cqes_per_reap
}
// impls: KqueueDriver (macOS dev tier) · UringDriver (linux + --features uring) · SimDriver (inf-sim)

// Executor (ADR-0003): !Send futures, Rc wakers (no atomics), slab tasks.
// NOTE: the original sketch (`poll_immediate -> PollImmediate<F>` returning
// the future on Pending) was unsound — a !Unpin future cannot move after its
// first poll. The shipped shape places the future into stable storage BEFORE
// the first poll and promotes in place; Ready still allocates nothing (reused
// scratch buffer + recycled header, no task slot).
pub struct CellExecutor;
pub struct TaskId;                          // {slot, generation}; stale ids detectable
pub enum PollImmediate { Completed, Suspended(TaskId) }
impl CellExecutor {
    pub fn new(capacity: usize) -> Self;
    /// Fast path: poll in place once; Completed ⇒ no slot, no malloc, no waker kept.
    pub fn poll_immediate<F: Future<Output = ()> + 'static>(&mut self, fut: F) -> PollImmediate;
    pub fn spawn_local<F: Future<Output = ()> + 'static>(&mut self, fut: F) -> TaskId;
    pub fn run_ready(&mut self, budget: usize) -> usize;   // tasks polled this slice
    pub fn live_tasks(&self) -> usize;                     // slab occupancy (leak assert)
    pub fn is_live(&self, id: TaskId) -> bool;
}
// Suspension primitives (typed, the only ways to suspend — `gate` module):
pub struct KeyedGate<K, V>;        // single-waiter primitive; complete() may precede first poll
pub type FabricGate<V> = KeyedGate<u64, V>;   // token-keyed; V = fabric reply payload
                                              // (inf-fabric is ABOVE this crate in the DAG)
pub type IoGate = KeyedGate<CompletionToken, CompletionResult>;  // M7 seam, exists at M0
pub struct WaitList<K>;            // key-keyed FIFO; wake_one/wake_all; baton-pass on drop
pub struct WatermarkGate;          // LSN-keyed; advance(lsn) wakes all ≤ lsn

// Reactor loop skeleton: the 10 steps with budgets + always-on iteration histogram.
pub trait CellPlane {
    fn on_completion(&mut self, cx: &mut LoopCx<'_>, c: Completion);   // 1 REAP dispatch
    fn on_timer(&mut self, cx: &mut LoopCx<'_>, key: u64) {}           // timer fired
    fn fabric_in(&mut self, cx: &mut LoopCx<'_>) {}                    // 2
    fn before_execute(&mut self, cx: &mut LoopCx<'_>) {}               // execute prelude, ADR-0152 A1
    fn parse_execute(&mut self, cx: &mut LoopCx<'_>);                  // 3+4
    fn maintain(&mut self, cx: &mut LoopCx<'_>) {}                     // 5 (stats flush at M0)
    fn seal_log(&mut self, cx: &mut LoopCx<'_>) {}                     // 6 (no-op at M0)
    fn respond(&mut self, cx: &mut LoopCx<'_>);                        // 7
    fn fabric_out(&mut self, cx: &mut LoopCx<'_>) -> bool { false }    // 8; true = work pending
}
pub struct LoopCx<'a> {            // ops pushed here ride the NEXT single submit (L3)
    pub now: Nanos,
    pub pool: &'a mut BufferPool, pub executor: &'a mut CellExecutor,
    pub timers: &'a mut TimerWheel,
    // push(IoOp) · budget(GroupClass) · charge(GroupClass, units) · note_fabric(msgs)
    // Private delivery and preparation budgets initialized once per native iteration.
}
pub struct ColdDeliveryBudget { remaining: u32 } // private, non-Copy; no public reset/constructor
pub struct ColdPreparationBudget { remaining: u32 } // same restrictions, separate allowance
impl LoopCx<'_> {
    pub fn cold_delivery_budget(&mut self) -> &mut ColdDeliveryBudget;
    pub fn cold_preparation_budget(&mut self) -> &mut ColdPreparationBudget;
}
pub struct CellLoop<D: BackendDriver, C: Clock>;
impl CellLoop {
    /// Backend-fatal errors propagate; per-op failures are completions.
    pub fn run_iteration(&mut self, plane: &mut impl CellPlane) -> io::Result<IterStats>;
    pub fn iteration_histogram(&self) -> &LogHistogram;    // cumulative loop buckets
    pub fn tripwires(&self) -> [(&'static str, u64); 5];   // frozen names, M0-S19 scrape
}

// Timer wheel v0 + scheduler groups v0 (E2 scope):
pub struct TimerWheel;  // 6×64 hierarchical, 1 ms tick; insert/cancel/advance/next_deadline
pub struct TimerId;     // generation-checked (stale cancels rejected)
pub enum GroupClass { Foreground, Maintenance }
pub struct GroupScheduler;  // deficit-weighted, burst-capped; refill/budget/charge
```

## 4. `inf-fabric` — ring, mesh, credits, codec v0

> **Accepted 2026-09-22, implementation open — ADR-0150:** bounded,
> resumable reply emission adds `Outcome::StreamReady`, `Op::StreamStep`
> and `Op::StreamResult`. Separate progress credits carry chunk pulls and
> terminal cancellation; returning the opening request's data credit does
> not release its stream resources. `StreamReady` is outcome tag 6
> carrying the stream's request token and owner generation; `StreamStep`
> (opcode 8) and `StreamResult` (opcode 9) carry a progress token, the
> stream, a sequence and an action or result, little-endian; generation
> zero, an unknown tag or an oversized chunk is a decode error. Owner
> generations and sequences never wrap: exhaustion refuses before any
> effect. A chunk is at most `REPLY_CHUNK_BYTES_MAX` = 65,536 encoded
> bytes and fits one frame, and a ring needs `capacity >= 2 ×
> (data_credits + progress_credits)`, one progress credit per ring
> (ADR-0150 D3, D4, A1). The codec and data-credit sketch below are the
> implemented shape: these additions are not built, and they bound no
> retained reply's bytes.

```rust
pub struct FabricToken(pub u64);           // {origin_cell:16, seq:48}; reply-routing key

// Codec v0 — frame header {version:u8, op:u8, flags:u16, len:u32}; payloads
// byte-exact round-trip (property-tested). Vocabulary:
pub enum Op<'a> {
    Read  { token: FabricToken, slot: KeySlot, key: &'a [u8] },
    Write { token: FabricToken, slot: KeySlot, key: &'a [u8], value: &'a [u8],
            expire_at: Option<Nanos>, flags: WriteFlags },
    /// Generic remote command execution, M0-experimental (M4 reshapes into Exec).
    /// args: ≤ MAX_APPLY_ARGS = 1024 slices — the client parser's argv bound
    /// (ADR-0120 D1); ≤ MAX_INLINE_APPLY_ARGS = 16 ride inline (no allocation),
    /// wider argvs one exact-sized table (D2). Wire layout unchanged (D3).
    Apply { token: FabricToken, slot: KeySlot, cmd: u8,
            args: /* ≤ MAX_APPLY_ARGS slices */, program: bool },
    /// Named namespace; defaults 0..16 use Apply (ADR-0015 D1). Same bound.
    ApplyNs { token: FabricToken, slot: KeySlot, cmd: u8, ns: u32,
              args: /* ≤ MAX_APPLY_ARGS slices */, program: bool },
    Batch { ops: /* nested Read/Write/Apply, one destination */ },
    Reply { token: FabricToken, outcome: Outcome<'a> },
    /// ADR-0128 (additive opcode 7): an accepted socket handed to
    /// another cell of the process; the adopter answers Reply { Ok }. Never
    /// inside a Batch, no program mark. Accepted ADR-0147 changes that reply's
    /// publication point to connection admission or terminal refusal close;
    /// incoming credit backs custody until then. This lifetime is unbuilt.
    AdoptConn { token: FabricToken, fd: u32 },
}
pub enum Outcome<'a> { Ok, Bytes(&'a [u8]), Int(i64), Nil, Bool(bool), Err(ErrCode) }
pub fn encode(op: &Op<'_>, out: &mut Vec<u8>);
pub fn decode(frame: &[u8]) -> Result<Op<'_>, CodecError>;

// SPSC ring: fixed power-of-two capacity, cache-padded indices,
// acquire/release only, batch publish/consume. Loom-modeled.
// Accepted ADR-0157 adds an owned-backing variant beside these types: a
// retained backing owner and two non-cloneable endpoints, fallible
// construction, eight slots for ADR-0156's cleanup jobs, capacity-first
// reservation and explicit budgeted retirement. Existing ring/mesh
// signatures are unchanged. The variant is unbuilt.
pub struct Producer<T>; pub struct Consumer<T>;
pub fn ring<T>(capacity: usize) -> (Producer<T>, Consumer<T>);
impl Producer<T> { pub fn try_push(&mut self, v: T) -> Result<(), T>;
                   pub fn publish_batch(&mut self, it: impl Iterator<Item=T>) -> usize; }
impl Consumer<T> { pub fn consume_batch(&mut self, max: usize, f: impl FnMut(T)) -> usize; }

// Mesh: N×(N−1) ring pairs + single-writer doorbells + credit flow control.
// (implemented — the code is the spec)
pub struct MeshConfig { pub ring_capacity: usize, pub data_credits: u32 }
   // construction asserts ring_capacity ≥ 2 × data_credits: the reserved
   // reply headroom that makes `reply` infallible (deadlock freedom).
pub enum SendError { NoCredit { needed: u32, available: u32 } }
pub struct Mesh;
pub struct CellFabric;                      // one per cell; moved to its thread
impl Mesh { pub fn new(cells: u16, cfg: MeshConfig) -> Vec<CellFabric>; }
impl CellFabric {
    pub fn cell(&self) -> CellId;
    pub fn next_token(&mut self) -> FabricToken;
    /// Stages toward `to`, consuming credits PER OP (Batch of k costs k).
    /// Err(NoCredit) ⇒ caller must backpressure (RecvDisarm the originating
    /// connection), never queue unbounded.
    pub fn send(&mut self, to: CellId, op: &Op<'_>) -> Result<(), SendError>;
    /// Credit-free (reserved headroom) — always sendable (deadlock freedom).
    pub fn reply(&mut self, to: CellId, token: FabricToken, outcome: &Outcome<'_>);
    pub fn flush(&mut self) -> usize;       // FABRIC-OUT: publish batches + doorbells
    /// FABRIC-IN. Reply frames return their credit BEFORE `f` sees them.
    pub fn drain(&mut self, max: usize, f: impl FnMut(CellId, Op<'_>)) -> usize;
    pub fn doorbell_pending(&self) -> bool;
    pub fn credits(&self, to: CellId) -> u32;       // backpressure probe (E5)
    pub fn outstanding(&self, to: CellId) -> u32;   // exact memory bound
    pub fn stats(&self) -> FabricStats;             // spill/orphan/publish tripwires
}
```

**Execution origin (ADR-0115 D2–D5).** `Apply` and `ApplyNs` carry
`program: bool`: `false` for forwarded client argv, `true` for legs composed
by a plane program. Codec v0 header bit 0 is `FLAG_PROGRAM = 1`, set iff
`program`; bits 1–15 remain reserved (`CodecError::ReservedFlags`). Bit 0
on any other opcode is `CodecError::FlagNotApplicable`; a Batch's children
carry their own origin. Namespace encoding and argument bounds are unchanged.

The send funnels require `ApplyOrigin::{Client, Program}`. Receivers
propagate the mark into `ConnCx::program`, including staged and
namespace-parked execution; local composed legs set the same context field.
Ordinary connections start with `program = false`. `CmdFlags::INTERNAL`
commands are refused as unknown before arity checks on client execution,
and hidden by client `COMMAND` introspection. Pre-registry program verbs
are intercepted only for marked execution. This is an execution class
between cells in one process, not authentication or an ACL capability
(ADR-0115).

## 5. `inf-wire` — RESP port + command metadata (implemented — the code is the spec)

> Deviation from the original sketch: `FrameIter` is a **lending**
> iterator — `next(&mut self) -> Option<Parsed<'_>>`, items borrow the
> iterator. The sketched plain `Iterator` was unsound: accumulator-backed
> frames could outlive accumulator maintenance. The lending shape also
> compiler-enforces the "frames never outlive EXECUTE unless copied"
> retention rule.

```rust
// Parser: resumable per-connection state over borrowed input; bounded
// accumulator (hard cap → typed error, the Vortex lesson). Multibulk frames
// parse with ZERO scanning (length-directed); payload bytes are never read.
pub struct ConnParser;                      // one per connection
pub enum Parsed<'a> { Command(ArgvRef<'a>), Inline(ArgvRef<'a>), Incomplete, ProtocolError(WireError) }
pub struct ParserLimits { pub max_bulk_bytes: usize, pub max_frame_bytes: usize, pub max_args: usize }
//  ADR-0122: `max_bulk_bytes` = `proto-max-bulk-len` (16 MiB default), checked from the
//  length line; `max_frame_bytes` = the whole frame (bulk cap + 64 KiB headroom),
//  checked from the declared layout — the accumulator bound is per frame, not per bulk.
impl ConnParser {
    pub fn new(limits: ParserLimits) -> Self;
    pub fn feed<'p>(&'p mut self, input: &'p [u8]) -> FrameIter<'p>;
    pub fn buffered(&self) -> usize;        // accumulator occupancy (bound asserts)
    pub fn is_poisoned(&self) -> bool;      // protocol error ⇒ close the connection
    pub fn limits(&self) -> ParserLimits;
    pub fn set_limits(&mut self, limits: ParserLimits); // CONFIG SET proto-max-bulk-len (ADR-0122)
}
impl FrameIter<'_> {
    /// Lending: `while let Some(p) = iter.next()`. Drive to None (or drop —
    /// unconsumed bytes carry to the next feed either way).
    pub fn next(&mut self) -> Option<Parsed<'_>>;
}
pub struct ArgvRef<'a>;   // argv[i] -> &'a [u8]; offset-based over the frame;
                          // no alloc ≤ 16 args (INLINE_ARGS), heap spill beyond

// Serializer: RESP2/RESP3 selected per connection (HELLO).
pub enum Protocol { Resp2, Resp3 }
pub struct RespWriter<'b>;                  // over &mut Vec<u8> (a wire buffer)
  // simple / error / error_bytes / int / bulk / null / null_array /
  // array_header / map_header / bool / double / verbatim / big_number —
  // RESP2/3 variants selected by `Protocol`; stack-buffer itoa, no allocation.
  // Line-framed replies (`simple`, `error`, `error_bytes`) SANITIZE their
  // text: a leading/trailing CR/LF run is trimmed and any remaining CR/LF
  // becomes a space, byte-for-byte as redis-server 8.0.5 does
  // (`sdstrim` + `sdsmapchars`). Amended 2026-09-01 by ADR-0097 — the
  // former contract ("text must not contain CR/LF; debug-asserted") was a
  // caller precondition that twelve live call sites violated with client
  // bytes, which let a client open a second RESP frame inside its own reply.
  // `error_bytes` takes raw argv bytes, which need not be UTF-8.
  // Length-prefixed replies are never sanitized.
  // `bulk_patched` (the M3 build-in-place bulk, ADR-0041 D10) has a TOTAL
  // header patch since 2026-09-01 (ADR-0099): the 8-digit
  // reserve is a fast path, not a bound — a payload needing more digits
  // takes a cold in-place widening. `try_bulk_patched` is the fallible
  // variant: a failing builder rolls the buffer back to the frame start
  // (no partial frame can escape), for reply-byte budgets that refuse
  // mid-serialization (`inf-doc`'s `serialize_*_bounded`,
  // `doc-max-reply-bytes`).
  // `mark` / `rollback` / `buffered_bytes` (ADR-0099 A1): `mark()` returns
  // a `ReplyMark` (not `Copy`) where one command's reply starts;
  // `rollback(&mark)` truncates everything written since and keeps earlier
  // pipelined replies; `buffered_bytes()` is the whole buffer's length. A
  // whole-reply byte account charges against the mark and rolls back to it
  // on refusal. `inf_wire::limits` owns the two frame bounds such an
  // account needs: `PATCHED_HEADER_SLACK_BYTES` (7, how far a patched
  // bulk's reserved header can shrink) and `DOUBLE_REPLY_BYTES_MAX` (344,
  // the widest `double` frame).

// Command metadata (frozen schema): name, arity, flags, key spec.
// EXPIREAT is not in the M0 surface (matches the S15 list); registry is the
// single growth point — the perfect-hash table is derived at compile time
// and the build FAILS on bucket collision.
pub enum CommandId { Ping, Echo, Hello, Get, Set, Setnx, Setex, Psetex, Getset, Getdel,
    Del, Exists, Type, Incr, Decr, IncrBy, DecrBy, Append, Strlen,
    Expire, Pexpire, Ttl, Pttl, Persist, Info, Command }
pub struct CommandMeta {
    pub id: CommandId, pub name: &'static str,
    pub arity: i8,                          // Redis convention: negative = at-least
    pub flags: CmdFlags,                    // READONLY | WRITE | ADMIN | FAST
    pub keys: KeySpec,                      // { first: u8, last: i8, step: u8 }; 0 = no keys
}
pub fn lookup(name: &[u8]) -> Option<&'static CommandMeta>;  // case-insensitive perfect hash:
    // fold+pack one u64, multiply-shift, one probe, one word compare (~5 ns dev-tier)
pub fn extract_keys<'v, 'a>(meta: &CommandMeta, argv: &'v ArgvRef<'a>) -> KeyIter<'v, 'a>;
pub fn key_spec(meta: &CommandMeta, subcommand: Option<&[u8]>) -> KeySpec;
    // ADR-0104: the routing truth — `meta.keys`, or the subcommand-scoped row
    // (`DEBUG OBJECT <key>` → SECOND; other DEBUG subcommands → NONE). Every
    // key-position consumer reads this, never `meta.keys` directly.
pub fn arity_ok(meta: &CommandMeta, argc: usize) -> bool;
```

### 5b. `inf-simd` (implemented — salvage layer)

```rust
pub fn swar_parse_int(buf: &[u8]) -> Option<(i64, usize)>;  // vortex-proto port, verbatim
pub fn scan_crlf(buf: &[u8]) -> CrlfPositions;   // SSE2/AVX2 ported; NEON path new (stable Rust)
pub fn find_crlf(buf: &[u8], from: usize) -> Option<usize>;
pub fn scalar_scan_crlf(buf: &[u8]) -> CrlfPositions;       // the proptest oracle
```

## 6. `inf-store` — records, index, ops, router (implemented — the code is the spec)

> **Accepted 2026-09-24, implementation open — ADR-0161:** fallible store
> construction and prepared materialization replaces the allocating
> Arena/CellStore constructors and implicit Keyspace materializers shown
> in this historical sketch. Checked plans and private owners prepare
> children, destination and cleanup capacity before one allocation-free
> publication; installed lookup cannot allocate. A replacement holds the
> old owners' cleanup slots and all new backing before one atomic swap,
> and a clear admits its empty representation and cleanup headroom with
> the store's capacity, never allocating after it releases the old
> contents. LFU sketches are prepared before the policy is published; a
> refusal keeps the old policy. Boot picks each tier's recovery life (the
> manifested watermark, or a fresh life) before it builds that tier's one
> table. A store resource refusal answers `-OOM store reservation refused`
> and changes neither the command's effects nor the connection's selection
> (ADR-0161 D3–D5, ADR-0172 D3). SELECT publishes its binding only after
> preparation succeeds. The signatures below are the implemented shape;
> the replacement above is not built.

> Deviations from the original sketch: the "8 B fixed" header is honored
> by narrowing `version` to **u24** (the sketch's field list summed to 72
> bits — it never fit u64; the 8 B header is load-bearing: the (16 B, 64
> B) gate record lands exactly in the 88 B size class with zero slack,
> putting measured overhead at 18.7 B/key). Mutating ops return `Result<_,
> OpError>` — arena-budget exhaustion (`OutOfMemory`) is backpressure the
> command layer must surface, never a panic. `append` returns `u64`,
> `strlen` returns `u64`.

```rust
// RecordHeader v0 — layout frozen:
//   type:4 | flags:4 | klen:u8 | vlen:u24 | version:u24   (8 B fixed)
//   [expire_at_ms: u40 if TTL flag] [key bytes] [value bytes]
pub struct CellStore;
impl CellStore {
    pub fn new(cfg: StoreConfig) -> Self;
    // Every op takes `now: Nanos` (expire-on-read; deterministic — L7).
    pub fn get(&mut self, key: &[u8], now: Nanos) -> Option<&[u8]>;
    pub fn set(&mut self, key: &[u8], value: &[u8], opts: SetOptions, now: Nanos)
        -> Result<SetOutcome, OpError>;
    pub fn del(&mut self, key: &[u8], now: Nanos) -> bool;
    pub fn exists(&mut self, key: &[u8], now: Nanos) -> bool;
    pub fn incr_by(&mut self, key: &[u8], delta: i64, now: Nanos) -> Result<i64, OpError>;
    pub fn append(&mut self, key: &[u8], tail: &[u8], now: Nanos) -> Result<u64, OpError>;
    pub fn strlen(&mut self, key: &[u8], now: Nanos) -> u64;
    pub fn getdel(&mut self, key: &[u8], now: Nanos) -> Option<Vec<u8>>;
    pub fn expire(&mut self, key: &[u8], at: Option<Nanos>, cond: ExpireCond, now: Nanos) -> bool;
    pub fn ttl(&mut self, key: &[u8], now: Nanos) -> Ttl;   // enum: Missing | NoExpiry | Ms(u64)
    pub fn type_of(&mut self, key: &[u8], now: Nanos) -> Option<TypeTag>;
    pub fn len(&self) -> usize;
    pub fn report(&self) -> MemoryReport;   // per-domain, byte-exact (L5)
}
pub struct SetOptions { pub cond: SetCond /* Always|IfAbsent|IfPresent */,
                        pub expire: SetExpire /* Keep|Clear|At(Nanos) */, pub get_old: bool }
pub enum SetOutcome { Applied { old: Option<Vec<u8>> }, Skipped { old: Option<Vec<u8>> } }
pub enum OpError { NotInt, Overflow, OutOfMemory, TooLarge }
pub enum ExpireCond { Always, IfNoExpiry, IfHasExpiry, IfGreater, IfLess } // EXPIRE NX/XX/GT/LT

// Batch prefetch pipeline (L3/L4) — hash+prefetch a parse batch, then execute.
// get_many is the full prefetch pipeline (probe-prefetch → candidate →
// record-prefetch → verify); it is OPT-IN per ADR-0005 (the +25% A/B gate
// missed on the bare-loop bench: +12.5%; end-to-end retest at S21).
impl CellStore {
    pub fn prefetch(&self, key_hash: u64);
    // M3-S20 / ADR-0044 additive extension: after record-line prefetch,
    // hint the first tape lines of a live JsonDoc fingerprint candidate.
    // Hint-only: exact-key verification remains in EXECUTE.
    pub fn prefetch_doc_root(&self, key_hash: u64);
    pub fn hash_key(key: &[u8]) -> u64;
    pub fn get_with_hash(&mut self, key: &[u8], hash: u64, now: Nanos) -> Option<&[u8]>;
    pub fn get_many(&mut self, keys: &[&[u8]], now: Nanos, out: impl FnMut(usize, Option<&[u8]>));
    pub fn probe_groups(&self, key: &[u8]) -> usize;   // diagnostics (histogram artifact)
}

// Slot router:
pub struct SlotRouter;
impl SlotRouter {
    pub fn new_contiguous(cells: u16) -> Self;             // static ranges (M0 topology)
    pub fn slot_of(key: &[u8]) -> KeySlot;                 // crc16(hashtag(key)) % 16384
    pub fn cell_of(&self, slot: KeySlot) -> CellId;
    pub fn is_local(&self, key: &[u8], cell: CellId) -> bool;
}
```

## 6b. `inf-server` — command execution (M0-S15; implemented core; pending changes marked)

> **Accepted 2026-09-22, implementation open — ADR-0150:** bounded,
> resumable reply emission replaces complete-buffer production replies
> with an admitted `ReplyPlan`: a proven bounded inline result or a
> continuation. It also replaces §6c's whole-reply observer input with
> bounded begin/chunk/end or abort events. Existing buffered writer
> rollback remains for bounded callers; streaming JSON measures its
> immutable source before publishing headers. A reply grant is reserved
> before the command's effects, and a result of unknown size never
> upgrades its grant after a mutation. An array header fixes the
> cardinality; each bulk header is measured from the same immutable source
> emission reads; an I/O failure after output starts closes the connection
> rather than send a partial reply. Command and effect order, argv order
> and PUBLISH's reply before its self-push stay. A stream frees its slot
> only when every custody is terminal, and a waiter's drop is not a
> cancellation acknowledgement (ADR-0150 D1, D2, D4, D5). These
> replacements are not built; the sketches below are the implemented
> execution and observer interfaces.

```rust
pub struct ConnCx { pub proto: Protocol, pub id: u64 }   // HELLO state
/// One parsed command → store ops → RESP2/3 reply bytes. All commands
/// enter through the inf-wire registry (lookup → arity → key spec).
pub fn execute(argv: &ArgvRef<'_>, store: &mut CellStore, cx: &mut ConnCx,
               now: Nanos, out: &mut Vec<u8>);
```

Reply bytes are oracle-pinned by the compat harness (`tests/compat`):
132 byte-exact cases vs real Redis 8.0.5; documented deviations:
`HELLO`/`INFO`/`COMMAND` payloads, `SET … EXAT/PXAT` (wall-clock timebase
arrives with the node), keys > 255 B / values > 16 MiB − 1 (record v0
bounds: a write naming one answers the typed error `ERR key or value
exceeds InfinityDB M0 record bounds`; a read or delete treats such a key
as absent — nil, `0`, `none`, `-2` — since no write can have stored it;
made precise 2026-09-16 when the simulator's independent model read the
sentence as "typed error on every command").

## 6c. `inf-server` — node assembly (M0 E6/E7 substrate; implemented)

```rust
/// One cell's complete data plane over any BackendDriver (uring in
/// infinityd, kqueue dev tier, SimDriver in inf-sim). Conn slab keyed
/// {slot:24, gen:32} (the completion-token model); local commands execute
/// synchronously (L6 fast path); remote-key commands run on a
/// per-connection ordered pump future (FabricGate replies, WaitList credit
/// backpressure, RecvDisarm past 1024 queued). Cross-cell = whole-argv
/// Op::Apply returning raw RESP bytes; DEL/EXISTS split per key (typed Int).
pub struct ServerPlane<O: PlaneObserver + 'static = NoopObserver>;
impl ServerPlane<O> {
    pub fn new(cell: CellId, cells: u16, listener: RawFd, store: CellStore,
               fabric: CellFabric, node: Rc<NodeInfo>, observer: O,
               route_local_only: bool) -> Self;   // route_local_only = routing-penalty A/B leg
    pub fn connections(&self) -> usize;
    pub fn suspended(&self) -> usize;             // sim quiescence probe
}

/// Apply-point hook: local execution + the owner side of remote Apply.
/// inf-sim's linearizability oracle consumes this; production observer is
/// a no-op that monomorphizes away. `scope` names the store the command
/// addressed (ADR-0105: `Db(db)` | `Ns(id)` | `Unavailable`, as the
/// connection stood when the command executed) so the replay model applies
/// the same argv to the same store.
pub trait PlaneObserver {
    fn on_execute(&mut self, cell: CellId, origin: ExecOrigin, scope: ExecScope,
                  argv: &[&[u8]], reply: &[u8], now: Nanos);
}
/// Quiescence content fold (ADR-0105): every live `(scope, key, value,
/// expiry deadline)` over the numbered dbs and the memory / flat-durable
/// named namespaces, through the store's checkpoint walk; tiered
/// namespaces are skipped and counted. `ServerPlane::fold_live_entries`
/// is the node side; the sim runs the same function on its model.
pub fn fold_live_entries(ks: &mut Keyspace, now: Nanos,
                         emit: impl FnMut(ExecScope, &[u8], &[u8], Option<u64>)) -> usize;

/// INFO-visible node stats (S19): frozen tripwire snapshot + raw lifetime
/// counters (scrapers diff two snapshots for under-load ratios) + memory
/// attribution domains the store can't see.
pub struct NodeInfo { /* Cells: tripwires, raw_counters, wire_buffers_bytes,
                         conn_state_bytes, connections, recv_dropped,
                         fabric_rtt_p50_ns, cell, cells */ }
```

`inf-runtime::net` (assembly helpers, keeps bins `forbid(unsafe_code)`):
`listen_reuseport(port) -> TcpListener` · `bound_port` ·
`pin_current_thread(core)`. `CellLoop` additionally exposes
`counters() -> [u64; 6]` (raw submits/sqes/cqes/iterations/commands/fabric)
— tripwire ratios are computed from windowed deltas, lifetime ratios
include idle parks.

**ADR-0136 (2026-09-17):** `NodeInfo::loop_snapshot` is a
cell-owned on-demand snapshot. The assembly calls
`capture_if_requested(iteration_histogram(), counters())` after each
iteration. It copies only on request into reusable storage. Explicit
`INFO server loophist` returns schema 1, cell/cells/run_id and either
`loop_histogram_pending:1` or `loop_histogram_samples`,
`loop_histogram_iterations`, `loop_histogram_submits`,
`loop_histogram_sqes`, `loop_histogram_counts` (1920 comma-separated
cumulative u64 counts, 32 sub-buckets/octave). It also requests the next
iteration-boundary snapshot. Ordinary INFO/all does not select this section.

The scraper retains a connection per cell and requires a newer sample
count after requesting each snapshot. Pairs must share cell/node identity,
have complete monotone buckets and counters, and contain samples and
submissions. Gate-run computes integer-rank p99.9 from bucket differences
around each pipelined load call, including warmup/drain and scrape RTTs;
the maximum across cells and replicates binds the unchanged <500 µs gate.
Bucket upper bounds conservatively round the result. Lifetime
`loop_iter_p999_us` remains diagnostic only; subtracting percentiles is
invalid. Raw snapshot pairs and per-window sample counts are retained in
the report; malformed, empty or stale windows are errors in either tier.

`LogHistogram` adds `BUCKET_COUNT`, `bucket_counts()`,
`bucket_upper_bound(index) -> Option<u64>` and allocation-free
`copy_from(&LogHistogram)` for this scrape. The schema's decimal list is
at most 40319 bytes; the benchmark bounds the entire reply at 64 KiB,
refuses duplicate fields, and fuzzes the decoder with `loop_histogram`.
`INFO tripwires:loop_snapshot_bytes` attributes the added bucket storage
(0 before a scrape, 15360 B after); it does not change the data maxmemory
policy, like the existing render-only pool gauges.

## 7. Tripwire counter set (`inf-foundation::tripwire`) — names frozen

**ADR-0137 (2026-09-17):** benchmark `ops` and `ops_per_sec`
count successful measured replies only; `errors` includes BUSY refusals.
Success and error latencies have separate histograms. `warmup_errors` is
separate from measured counts. Every M0/M1/M2 native load leg, fill and
generator probe refuses errors (including warmup) before accepting a gate
measurement. This changes instrument semantics, not numeric thresholds.

`sqes_per_submit` · `cqes_per_reap` · `cmds_per_iter` · `fabric_msgs_per_batch`
· `loop_iter_p999_us` · memory domains: `records_live_bytes`,
`records_slack_bytes`, `index_bytes`, `wire_buffers_bytes`, `conn_state_bytes`,
`process_rss`.

---

**M1 extension note (2026-06-12, ADR-0008):** the registry grew to 57
commands through its designed growth point (two-word perfect hash,
`MAX_NAME_LEN = 16`, 256 buckets — internal detail; `CommandMeta`/`lookup`
shapes unchanged). Additive deltas: `OpError` + `NotFloat | NanOrInf`;
`MemoryReport` + `wheel_bytes` (the M1-S04 TTL-wheel attribution domain);
`KeySpec::{TWO, PAIRS, SECOND}`; `NodeInfo` + wall-clock anchor / RNG state /
client registry / CONFIG store; new `inf-store` inherent methods for the
M1-E1 ops and `expire_tick` (M1-E2). Deadlines past the u40-ms record bound
now clamp (previously a latent panic; ADR-0008).

**M1-S04 expiry schedule note (2026-09-30, ADR-0008 A1):** one wheel node
per key hash with a deadline, and a key the node budget refuses is swept,
never left to lazy expiry. All deltas additive. `StoreConfig` +
`wheel_nodes_max` (`WheelNodesMax`, checked against `WHEEL_NODES_MAX`;
tests and the simulator lower it). `ExpiryBudget` + `max_sweep_slots` (one
keyspace-wide sweep budget per slice) and `ExpiryBudget::UNBOUNDED`.
`ExpiryStats` + `fired` (the unit of `max_fires`), `refiled`, `swept`,
`sweep_slots`, `sweep` (`SweepState`), `sweep_stop` (`SweepStop`),
`tombstones` and `fires_charged()` (fires plus sweep reaps: what a slice
spends of its fire budget). `StoreStats` + `expired_swept`,
`wheel_refiled`, `sweep_passes_voided`, `expiry_alias_over` and the
`wheel_tombstones` gauge. `MemoryReport` + `wheel_live_bytes` (nodes in use,
the pressure comparable's wheel term; `wheel_bytes` stays the resident
attribution). `Index::rebuilds`; `CellStore::expiry_settled` and
`Keyspace::expiry_settled` (the frozen-time drain predicate). New `limits`
rows: `WHEEL_NODES_MAX`, `WHEEL_MEMBER_SHARDS`,
`EXPIRY_SWEEP_SLOTS_PER_SLICE`, `EXPIRY_SWEEP_CHUNK_SLOTS`,
`WHEEL_TOMBSTONES_MAX`.

**M1-E3/E4 extension note (2026-06-12, ADR-0009):** two frozen signatures
changed shape — `execute(...)` and `ServerPlane::new(...)` now take
`inf_store::Keyspace` (one cell's slice of every namespace: 16 lazily
materialized default dbs + named-ns registry + pressure driver) instead of
`CellStore`, whose own frozen method set was unchanged behind
`Keyspace::db_mut(n)` at M1. §6's replacement of that implicit
construction crossing (ADR-0161) is not built. `ConnCx` gains `db: u16`.
Additive deltas: registry 57 → 58 (`INF.NS`); `CmdFlags::DENYOOM` (the
M1-S07 OOM gate enters through metadata); `MemoryReport` +
`evict_bytes`; `StoreStats` + `evicted_keys`; `StoreConfig` +
`evict_seed`; new `inf-store` types `Keyspace` / `PressureConfig` /
`EvictBudget` / `EvictionPolicy` / `EvictStats` / `NsMode` / `NsSpec` /
`NsError`; `Index::live_walk` (read-only clock-hand iteration). The
fabric codec is unchanged; `Op::Apply`'s `cmd` byte packs
`{db:4 | proto:4}` (old encodings decode as db 0). The record header's
two spare flag bits became the CLOCK reference counter (layout
untouched; ADR-0009).

**M1-E5 extension note (2026-06-12, ADR-0010):** all deltas additive. The
registry grew 58 → 64 (SUBSCRIBE/UNSUBSCRIBE/PSUBSCRIBE/PUNSUBSCRIBE/
PUBLISH/PUBSUB; channels are not keys — `KeySpec::NONE`); the perfect-hash
multipliers were re-searched (the build's collision proof fired as
designed). `RespWriter` + `push_header` (RESP3 push / RESP2 array).
`ConnCx` + `sub_channels`/`sub_patterns` (subscription state, the ADR-0009
`db` pattern). `NodeInfo` + pub/sub gauges/counters and
`client_output_buffer_limit_disconnections`. CONFIG store +
`client-output-buffer-limit` (class-triple merge kind). The fabric codec is
unchanged: the pub/sub fan-out vocabulary (`INF.PUB`/`INF.PUBFAN`/
`INF.SUBD`/`INF.PUBSUB`) rides `Op::Apply` as unregistered argv programs
intercepted by the plane ahead of `execute` — invisible to clients,
reserved names for the fabric (ADR-0010). **ADR-0101 (2026-09-01):** `INF.PUB`
and the owner's `INF.PUBFAN` leg *to the origin cell* carry two optional
trailing arguments — the publisher tag `conn seq` (the origin's packed
connection key and its remote-publish sequence, decimal, opaque to the
owner) — present iff the publishing connection holds a subscription. The
tagged leg stashes the publisher's own frames on that connection; the pump
emits them right after the count reply, so the remote-owner arm now
matches Redis's reply-then-push order. Every other leg keeps the three-arg
form; the codec is untouched.

**Linux-validation note (updated 2026-06-11):** the io_uring backend is now
exercised on real Linux (kernel 7.0): conformance suite green in probed
(multishot + provided buffers) and `INF_URING_FORCE_DEGRADED` modes, 1M-cycle
lifecycle storms reconcile in both, and the S04 echo gate passes ×30+.
The first live run found and fixed three driver bugs. Still pending: the
5.15/6.1 kernel-matrix CI legs and the reference-box gate campaign (S21).


## M1-S02 move primitives — ADR-0110 amendment (2026-09-05)

The registered internal commands retain their two-argument forms and add:

- `INF.PEEK key ABS`: non-destructive string snapshot `[value, unix_expiry_ms]`,
  expiry `-1` for persistent, null array for missing, WRONGTYPE for non-string.
- `INF.TAKE key IF value unix_expiry_ms`: integer 1 if bytes and absolute
  expiry both match and the owning cell deletes synchronously, otherwise 0.
  Deadlines accept `-1` or a nonnegative signed 64-bit millisecond timestamp.
  Bad shape/options/deadlines return a typed error before any mutation.

The cross-cell string move runs snapshot → SET [PXAT] [NX] → conditional
cleanup; COPY omits cleanup. Only exact `+OK` authorizes deletion, and the
comparison/removal shares one execution instant. A refused put preserves the
source; refused/failed cleanup can retain a copy and reports failure. Independent
expiry/eviction/writes keep their effects; identical-content ABA is not detected.
No borrowed store state crosses suspension; Apply/RESP codecs and persisted
formats are unchanged. Full atomicity stays M6; named-namespace cross-owner
moves retain ADR-0015's refusal. One additional bounded source leg, no shared
state and no change to ordinary GET/SET paths (Correctness-only).

ADR-0110 first amendment (2026-09-07): snapshots require exact framing,
checked length arithmetic and no trailing bytes. `INF.PEEK key ABS NOSTATS`
and conditional TAKE omit client hit/miss counters; COPY retains counted ABS.
`CellStore::peek_str` keeps string type checks and expiry/access resolution
without those counters. The SET builder represents optional expiry with an
`Option`, and accepts zero from injected anchors. Changed-source cleanup
returns `-BUSY source changed during cross-cell move; destination may contain
a copy`; RENAMENX retry may return `:0` against that copy. The hidden
`parse_take_reply` export exists for direct decoder fuzzing.

ADR-0110 third amendment (2026-09-08): a third registered internal
command, `INF.PUT key value unix_expiry_ms [NX]` — the RENAME/RENAMENX
destination leg. Registry flags are RENAME's (`write`, no `denyoom`); the put
is `CellStore::set` (store bounds typed, arena exhaustion answers OOM, any
type at the destination is overwritten as RENAME does); the deadline is `-1`
or a positive Unix-ms timestamp (otherwise `ERR invalid move snapshot
deadline`, before any mutation); replies are SET's (`+OK`, null on a refused
NX). The move runs snapshot → `INF.PUT … [NX]` → conditional cleanup for the
renames; COPY keeps `SET [PXAT] [NX]` and therefore COPY's `denyoom`. Deadline
arithmetic at the command seam is ADR-0111: every expire argument is decided
in i64 Unix milliseconds exactly as Redis decides it, and every accepted
instant saturates into the record's u40-ms bound (`inf_store::MAX_EXPIRE_MS`,
`inf_store::saturating_deadline` — now public); the ADR-0008 clamp is the
declared deviation for read-backs of a saturated deadline. An instant before
the internal clock's origin has no u40 deadline and is never clamped onto
the origin, where it would read as live for that millisecond: it converts to
`inf_store::InternalDeadline::BeforeOrigin`, expired at every reading of the
clock, and each store write that receives it leaves no key — `SET`'s
overwrite is a delete, `GETEX` answers and then deletes, the `EXPIRE` family
deletes, replay deletes.
