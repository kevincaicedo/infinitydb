# M0 Interface Freeze

Authoritative Rust signatures for the cross-crate seams frozen at M0 exit
(milestone doc §3.2). Changing one of these after M0 requires an ADR.
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
pub fn hash64(data: &[u8], seed: u64) -> u64;      // wyhash-style, stable
pub fn crc16(data: &[u8]) -> u16;                  // XMODEM, Redis Cluster vectors
pub fn hashtag(key: &[u8]) -> &[u8];               // Redis Cluster {tag} rule
pub mod varint { encode_u64 / decode_u64 }
pub struct LogHistogram;                   // record(u64) / percentile(f64) / max / count
pub struct CachePadded<T>(pub T);          // #[repr(align(128))]
pub struct LocalCounter;                   // Cell<u64>: no atomics (L1)
pub mod tripwire { /* frozen counter names, M0 §3.2 */ }
```

## 2. `inf-alloc`

```rust
// Buffer pool (implemented — wire buffers, registered with the backend).
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
    pub fn reconcile(&self) -> Result<(), LeaseLeak>; // leak test hook
}

// Record arena (agent-built): size-class slab allocator over mmap chunks.
pub struct Arena;
pub struct ArenaAddr(/* private; 48-bit addressable */);
impl Arena {
    pub fn new(config: ArenaConfig) -> Self;
    pub fn alloc(&mut self, len: usize) -> Option<ArenaAddr>;   // None = budget exhausted
    pub fn free(&mut self, addr: ArenaAddr, len: usize);
    pub fn bytes(&self, addr: ArenaAddr, len: usize) -> &[u8];
    pub fn bytes_mut(&mut self, addr: ArenaAddr, len: usize) -> &mut [u8];
    pub fn report(&self) -> ArenaReport;   // live_bytes, slack_bytes, resident_bytes — byte-exact
}
```

## 3. `inf-runtime` — backend driver + executor + loop (implemented — the code is the spec)

> Deviations from the original sketch are deliberate and recorded in
> `reviews/infinity-m0-skeleton.md` §"Interface deviations": `generation()`
> (edition-2024 keyword), the Pin-sound `PollImmediate` shape,
> `FabricGate<V>`, `submit_stats()`/`performance_tier`, fallible
> `run_iteration`, `CellPlane::on_timer`.

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
    /// Multishot accept: one arm yields Accepted completions until disarmed/error.
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
}
pub struct CellLoop<D: BackendDriver, C: Clock>;
impl CellLoop {
    /// Backend-fatal errors propagate; per-op failures are completions.
    pub fn run_iteration(&mut self, plane: &mut impl CellPlane) -> io::Result<IterStats>;
    pub fn iteration_histogram(&self) -> &LogHistogram;    // loop_iter p999 gate
    pub fn tripwires(&self) -> [(&'static str, u64); 5];   // frozen names, M0-S19 scrape
}

// Timer wheel v0 + scheduler groups v0 (E2 scope):
pub struct TimerWheel;  // 6×64 hierarchical, 1 ms tick; insert/cancel/advance/next_deadline
pub struct TimerId;     // generation-checked (stale cancels rejected)
pub enum GroupClass { Foreground, Maintenance }
pub struct GroupScheduler;  // deficit-weighted, burst-capped; refill/budget/charge
```

## 4. `inf-fabric` — ring, mesh, credits, codec v0

```rust
pub struct FabricToken(pub u64);           // {origin_cell:16, seq:48}; reply-routing key

// Codec v0 — frame header {version:u8, op:u8, flags:u16, len:u32}; payloads
// byte-exact round-trip (property-tested). Vocabulary:
pub enum Op<'a> {
    Read  { token: FabricToken, slot: KeySlot, key: &'a [u8] },
    Write { token: FabricToken, slot: KeySlot, key: &'a [u8], value: &'a [u8],
            expire_at: Option<Nanos>, flags: WriteFlags },
    /// Generic remote command execution, M0-experimental (M4 reshapes into Exec).
    Apply { token: FabricToken, slot: KeySlot, cmd: u8, args: /* ≤ MAX_APPLY_ARGS slices */ },
    Batch { ops: /* nested Read/Write/Apply, one destination */ },
    Reply { token: FabricToken, outcome: Outcome<'a> },
}
pub enum Outcome<'a> { Ok, Bytes(&'a [u8]), Int(i64), Nil, Bool(bool), Err(ErrCode) }
pub fn encode(op: &Op<'_>, out: &mut Vec<u8>);
pub fn decode(frame: &[u8]) -> Result<Op<'_>, CodecError>;

// SPSC ring: fixed power-of-two capacity, cache-padded indices,
// acquire/release only, batch publish/consume. Loom-modeled.
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

## 5. `inf-wire` — RESP port + command metadata (implemented — the code is the spec)

> Deviation from the original sketch (recorded in
> `reviews/infinity-m0-skeleton.md`): `FrameIter` is a **lending** iterator —
> `next(&mut self) -> Option<Parsed<'_>>`, items borrow the iterator. The
> sketched plain `Iterator` was unsound: accumulator-backed frames could
> outlive accumulator maintenance. The lending shape also compiler-enforces
> the "frames never outlive EXECUTE unless copied" retention rule.

```rust
// Parser: resumable per-connection state over borrowed input; bounded
// accumulator (hard cap → typed error, the Vortex lesson). Multibulk frames
// parse with ZERO scanning (length-directed); payload bytes are never read.
pub struct ConnParser;                      // one per connection
pub enum Parsed<'a> { Command(ArgvRef<'a>), Inline(ArgvRef<'a>), Incomplete, ProtocolError(WireError) }
impl ConnParser {
    pub fn new(limits: ParserLimits) -> Self;
    pub fn feed<'p>(&'p mut self, input: &'p [u8]) -> FrameIter<'p>;
    pub fn buffered(&self) -> usize;        // accumulator occupancy (bound asserts)
    pub fn is_poisoned(&self) -> bool;      // protocol error ⇒ close the connection
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
  // simple / error / int / bulk / null / null_array / array_header /
  // map_header / bool / double / verbatim / big_number — RESP2/3 variants
  // selected by `Protocol`; stack-buffer itoa, no allocation.

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
pub fn arity_ok(meta: &CommandMeta, argc: usize) -> bool;
```

### 5b. `inf-simd` (implemented — salvage layer)

```rust
pub fn swar_parse_int(buf: &[u8]) -> Option<(i64, usize)>;  // vortex-proto port, verbatim
pub fn scan_crlf(buf: &[u8]) -> CrlfPositions;   // SSE2/AVX2 ported; NEON path new (stable Rust)
pub fn find_crlf(buf: &[u8], from: usize) -> Option<usize>;
pub fn scalar_scan_crlf(buf: &[u8]) -> CrlfPositions;       // the proptest oracle
```

## 6. `inf-store` — records, index, ops, router

```rust
// RecordHeader v0 (master plan §7.2) — layout frozen:
//   type:4 | flags:4 | klen:u8 | vlen:u24 | version:u32   (8 B fixed)
//   [expire_at_ms: u40 if TTL flag] [key bytes] [value bytes]
pub struct CellStore;
impl CellStore {
    pub fn new(cfg: StoreConfig) -> Self;
    // Every op takes `now: Nanos` (expire-on-read; deterministic — L7).
    pub fn get(&mut self, key: &[u8], now: Nanos) -> Option<&[u8]>;
    pub fn set(&mut self, key: &[u8], value: &[u8], opts: SetOptions, now: Nanos) -> SetOutcome;
    pub fn del(&mut self, key: &[u8], now: Nanos) -> bool;
    pub fn exists(&mut self, key: &[u8], now: Nanos) -> bool;
    pub fn incr_by(&mut self, key: &[u8], delta: i64, now: Nanos) -> Result<i64, OpError>;
    pub fn append(&mut self, key: &[u8], tail: &[u8], now: Nanos) -> Result<u32, OpError>;
    pub fn strlen(&mut self, key: &[u8], now: Nanos) -> u32;
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

// Batch prefetch pipeline (L3/L4) — hash+prefetch a parse batch, then execute:
impl CellStore { pub fn prefetch(&self, key_hash: u64); pub fn hash_key(key: &[u8]) -> u64; }

// Slot router:
pub struct SlotRouter;
impl SlotRouter {
    pub fn new_contiguous(cells: u16) -> Self;             // static ranges (M0 topology)
    pub fn slot_of(key: &[u8]) -> KeySlot;                 // crc16(hashtag(key)) % 16384
    pub fn cell_of(&self, slot: KeySlot) -> CellId;
}
```

## 7. Tripwire counter set (`inf-foundation::tripwire`) — names frozen

`sqes_per_submit` · `cqes_per_reap` · `cmds_per_iter` · `fabric_msgs_per_batch`
· `loop_iter_p999_us` · memory domains: `records_live_bytes`,
`records_slack_bytes`, `index_bytes`, `wire_buffers_bytes`, `conn_state_bytes`,
`process_rss`.

---

**Linux-validation note:** the `BackendDriver` contract is exercised on macOS
(kqueue) and in `inf-sim`; the io_uring implementation compiles under
`--features uring` on Linux targets but its multishot/provided-buffer paths,
kernel-matrix degradation, and all performance gates require the Linux
reference box (tracked in `reviews/infinity-m0-skeleton.md`).
