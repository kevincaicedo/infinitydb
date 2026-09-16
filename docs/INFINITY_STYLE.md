# InfinityStyle

> Inspired by TigerBeetle's
> [TIGER_STYLE](https://github.com/tigerbeetle/tigerbeetle/blob/main/docs/TIGER_STYLE.md),
> adapted to Rust, and InfinityDB's
>
> Normative for all code in this workspace. Reviewers affirm conformance
> (ADR-0025). This document states enduring engineering rules.

## Why Have Style?

Another word for style is design. Our design goals are **safety,
performance, and developer experience — in that order**. All three matter;
the order settles arguments. Style is not readability cosmetics: it is the
set of decisions that make the next thousand decisions cheaper and safer.

The unit of correctness and performance is the whole system. An elegant
component that complicates recovery, hides memory, or stalls another cell
is an incomplete design. Every rule needs a reason that a reviewer can
explain and a way to establish conformance.

## Simplicity and Elegance

Simplicity takes design work and revision. Spend that effort before code:
sketch ownership, state transitions, failure paths, and resource budgets.
A small design that brings safety, performance, and clarity together is
worth more than a clever implementation of a complicated design.

- **Minimize concepts and states.** Prefer one owner, one authoritative
  representation, and one path for each state transition. Remove special
  cases before adding flags that interact with them.
- **Use a few strong abstractions.** A useful abstraction captures a domain
  invariant, narrows an API, or makes an ownership boundary explicit. Avoid
  speculative frameworks, wrapper layers that only rename calls, and
  generic machinery for capabilities we do not need.
- **Keep the proof local.** A reviewer should be able to explain why a
  mutation is valid, what it costs, and what failure leaves behind without
  reconstructing distant caller assumptions. Prefer explicit code over a
  shorter expression that hides order, allocation, or error handling.
- **Delete complexity when replacing it.** Remove obsolete paths, flags,
  comments, and configuration together. Keep compatibility paths only for
  a defined contract with tests; a second implementation doubles what must
  remain correct.

## Zero Technical Debt

**Zero technical debt.** Do not merge known correctness defects, broken
invariants, unbounded work, or design shortcuts that require a later repair.
Fix the root cause while the design is understood. If the scope is too
large to finish correctly, reduce the scope to a complete, tested contract.

Recording debt does not make it acceptable. An issue, `TODO`, feature flag,
or `Evidence-pending` label is no permission to ship a known bug. When a
defect is discovered, preserve a reproducer, repair it, and retain the
regression test before calling the affected work complete. Do not build
new behavior on a known broken foundation.

An intentionally unsupported feature with a defined refusal is a product
boundary. An unrun benchmark is an evidence gap. Neither licenses a
violation of the behavior we promise. Existing findings remain open until
their fixes are verified; changing this policy does not close them.

## Safety

The [NASA Power of Ten](https://spinroot.com/gerard/pdf/P10.pdf) and
TigerStyle inform these rules. Apply their purpose to our ownership model,
resumable commands, and variable-size inputs.

### Control flow

- **Simple, explicit control flow only.** Prefer `?`, `let-else`, early
  returns, and small helpers over nested pyramids. Keep phase selection and
  mutation order in the parent; give leaf helpers narrow inputs and a
  single job. Separate policy decisions from repeated work. Prefer pure
  helpers where they make the result easier to verify.
- **No recursion in decoders or on the data plane.** Every parser (RESP,
  JSON, JSONPath, PartiQL, log frames, cursors, fabric codec) is iterative
  and bounded; nested grammars use an explicit stack with a depth limit.
  A grammar whose nesting the type forbids beyond one level (a fabric
  batch, a JSONPath descend) is a two-level walk — a leaf function and a
  caller that loops it — never a self-call, and its encoder and printer
  take the same shape (ADR-0125 A4). Its fuzz target lands **in the same
  PR** (L9). Recursion elsewhere needs a proven bound and a reviewer who
  agrees; the census's remaining rows and their classification are in
  ADR-0125 A4.
- **Function limit: 70 code lines.** Split by responsibility, keeping the
  transition visible and the helper contract meaningful. ADR-0125 defines
  the mechanical scope:
  `scripts/check-fn-length.sh` runs clippy's `too_many_lines` at 70 code
  lines on every production target and ratchets
  `docs/fn-length-baseline.tsv` — a file's count never goes up, and the
  row comes down with the code. A function opts out only with
  `#[allow(clippy::too_many_lines, reason = "…")]`, and every opt-out is
  printed on the gate's OK line. The baseline records existing violations
  to remove; it is no budget for new ones. Never trade one oversized
  function for another merely because the per-file count stays level.
- **Hard limit: 3000 production lines per file.** Tests do not count
  (`strip-test-modules.awk` decides, exactly as the panic-policy gate
  sees the tree). Over the bar, a file becomes a folder: behaviour moves
  into child modules, `pub(super)` marks what crosses, and the
  parent re-exports every public path so callers never move.
  `scripts/check-file-length.sh` enforces it (ADR-0125).
- **Every command is a resumable state machine (L6).** Suspension points are
  few, typed, and visible. Never hold buffer leases, response iovecs, ring
  slots, arena borrows, staged log records, or command-local guards across a
  suspension, requeue, or reactor-iteration boundary — this is our
  place-of-check-to-place-of-use rule, and it is checked in review every
  time. A single-threaded cell still permits interleaving across a yield.
  Retain only state that the seam explicitly permits, such as owned bytes,
  typed identities, or bounded reservations; reacquire and revalidate
  mutable facts when execution resumes.
- **Don't react to external events directly; run at your own pace.** The
  reactor reaps, drains, parses, executes, and submits in **batches** under
  budgets (L3). Code that does per-event work at a boundary — one syscall
  per op, one wake per message, one fsync per write — is architecturally
  wrong even when it benchmarks fine at low load. A batch may contain one
  item at low traffic; flush and fairness deadlines still apply.
- **Make progress explicit.** Each state-machine step consumes input,
  advances a cursor, spends a bounded budget, waits for a named event, or
  terminates. Define who schedules the next step. Requeueing unchanged work
  without a wakeup condition is a busy loop.

### Put a limit on everything

Every loop has a bound or a documented event-loop lifetime with bounded
work per iteration. Every queue, ring, cache, page, batch, retry sequence,
and backlog has a cap and a defined full-state behavior. Configuration
validates limits and their arithmetic before allocating or serving.

- Bound **bytes and work as well as item counts**. A batch of a few large
  values can monopolize a core. Bound fan-out, response expansion, nesting,
  scratch space, and the bytes retained while waiting.
- Use credits, budgets, output caps, and the contract's admission or pacing
  policy. Never replace backpressure with an unbounded queue or spin retry.
  Distinguish an impossible request from one waiting for temporary capacity.
- Bound each retry and define its deadline, cancellation, and progress
  condition. A timeout ends waiting; it does not prove that a mutation did
  not execute.
- At a coverage limit, return a continuation, a typed refusal, or an
  explicitly partial result as the API defines. Silent truncation cannot
  stand in for a complete result (L10).

### Allocation and resource ownership

**Variable-size allocation is part of the workload.** Keys, values,
documents, and replies are not fixed-size records. Allocate and reclaim
their storage through bounded, accounted ownership. Do not pay worst-case
capacity for every small value.

- Preallocate fixed control structures and reusable scratch where the
  capacity is known. Use cell-owned arenas, slabs, and pools for admitted
  variable-size data. Variable payloads do not excuse incidental allocation
  in hot paths. New global-allocator reliance or per-item scratch growth
  requires a named acceptance criterion and an A/B; admitted payload
  allocation through the owning domain is normal operation.
- Validate length, checked size arithmetic, representation overhead, and
  quota before reserving. Attribute capacity, metadata, alignment, slack,
  and retained buffers to named domains; logical value length alone is
  not the memory bill. Count reservations once throughout their lifetime.
- **Reserve before publishing.** Know the worst-case resources needed to
  finish a mutation before changing visible state. Handle expected capacity
  exhaustion through fallible APIs. An allocation failure must not expose
  half a replacement or destroy the value being replaced.
- Define who releases every allocation, credit, token, and handle on
  success, refusal, cancellation, disconnect, and shutdown. Use RAII for
  synchronous cleanup; put cleanup requiring I/O in an explicit state
  machine. Do not rely on destructors to establish crash durability.
- Growth, shrinking, copying, rehashing, and destruction all cost work.
  Bound those costs per slice, including the temporary peak when old and
  new representations coexist. Amortized complexity does not bound the
  latency of one resize or one large drop.

### Types

- **Make invalid states unrepresentable**: newtypes for ids and offsets,
  enums for state machines, typestate for phase-ordered resources (the
  buffer/token lifecycles), generation tokens where ABA lurks. A bug the
  type system catches costs nothing forever.
- **Explicitly sized integers** (`u32`, `u64`, `i64`) in every wire format,
  log record, index slot, and counter. `usize` is for in-memory indexing,
  lengths, and capacities; it never crosses a serialization boundary.
- Specify byte order, format version, valid flags, and length semantics.
  Encode fields explicitly; Rust struct layout and enum representation are
  not storage formats. Encoder and decoder limits must agree, including
  headers, trailers, alignment, and the largest representable record.
- Distinguish `index`, `count`, and `size` in names and in casts — the
  off-by-one trio. Show division intent: `div_ceil`, explicit floor,
  or a comment proving exactness.
- **Use the smallest type that preserves the contract.** Return `Result`
  when an operation can fail, `Option` for legitimate absence, and an enum
  for distinct outcomes. Never collapse corruption, refusal, absence, and
  success into a boolean or sentinel to simplify a call site. Avoid
  wildcard matches that silently accept a new state or error variant.
- Prevent argument swaps with distinct newtypes or a named options struct
  when parameters share a representation but have different meanings.
  Prefer an enum to positional booleans that select unrelated modes.
- Narrow integers with `try_from` or a locally proven bound. Choose
  checked, saturating, or wrapping arithmetic deliberately. Saturation may
  suit an approximate metric; it must not hide broken quota accounting.
  Identity and generation wrap require a reuse proof or a typed refusal.
- Define floating-point behavior for NaN, infinities, signed zero, ordering,
  and canonical encoding where relevant. State whether the contract needs
  exact bits, a total ordering, or a numerical tolerance; do not assume
  those are interchangeable.

### Assertions

Assertions detect programmer errors; operating errors get handled, never
asserted. The only correct response to corrupt code is to crash — assertions
downgrade catastrophic correctness bugs into liveness bugs, and they are a
force multiplier for DST and fuzzing.

- **Assert arguments, return values, preconditions, postconditions, and
  invariants.** Aim for two meaningful checks per data-plane function on
  average; type-enforced guarantees do not need ceremonial assertions to
  meet a quota. Check relationships, legal transitions, and impossible
  combinations, not just individual field ranges.
- `debug_assert!` is the default for internal diagnostic checks. Deliberate
  promotion to release `assert!` protects invariants whose violation
  endangers durable state. Per-operation promotions carry a ≤ 1% A/B;
  per-batch or per-checkpoint checks record that scope and its cost.
  Neither input validation nor an unsafe operation's necessary safety
  precondition may depend solely on a check absent from release builds.
- **Pair checks across boundaries.** Assert what an internal producer
  emits; validate what a consumer receives. A decoder returns a typed error
  for bad external bytes, even if our writer would assert before emitting
  them. Independently check enqueue/dequeue, encode/decode, and
  reserve/release relationships so the same mistaken assumption does not
  validate itself twice.
- Check the states we allow **and the states we forbid**. Test transitions
  across that boundary, including exact limits, stale identities, and
  repeated completion. An assertion on the happy path alone is incomplete.
- **Split compound assertions** — `assert!(a); assert!(b);` reads better and
  fails more precisely than `assert!(a && b)`.
- **Assert compile-time relationships** with const assertions (record
  header sizes, ring slot alignment, enum discriminant ranges). A design
  error caught before the program runs is the cheapest bug of all.
- Every state machine keeps a written **invariant inventory** (what holds,
  where it is established, and how it is enforced) in its interface
  documentation. Identify proof gaps as open findings; the inventory is
  no waiver for a missing safety check. Update it with the transition.
- Assertions are a safety net, not a substitute for understanding. Build
  the mental model first, encode it in assertions, explain it in comments,
  and let the simulator hunt what both of you missed.

### Panics and errors

- **Panics are for violated internal invariants and sanctioned fail-stop
  conditions only.** Input validation, protocol errors, admission pressure,
  and recoverable I/O failures return typed errors. The CI panic-policy
  grep enforces the letter; you enforce the spirit.
- **All errors are handled.** An ordinary error path can cause a
  catastrophic failure. Give those paths tests, fault points, and
  crash-matrix coverage. A DST arm or fault plant asserts its own
  **engagement** when added: a condition that never fired is `VACUOUS`,
  never a green result (ADR-0117). Preserve engagement counters across
  simulated restarts. `unwrap()`/`expect()` on an operational `Result` is
  a review reject; an invariant-justified `expect()` is judged as an
  assertion. Every release `assert!`/`expect()`/`panic!`/`unreachable!`
  in cell code is a row of `docs/release-assert-inventory.tsv` (ADR-0107
  D2): `I` an invariant on the callee's own state, `C` a claim about a
  caller **with the enforcing check named as a symbol the gate resolves**
  (`path/from/root.rs:Type::method` — a renamed enforcing function is
  red; ADR-0107 first amendment), `F` a fail-stop the policy sanctions.
  An ordinary operating error cannot become an invariant by adding a row:
  flags, client-sized numbers, and lifetime counters need validated bounds
  or a defined exhaustion/wrap policy before they reach an assertion.
  `check-release-asserts.sh` is red on a new, changed or vanished site
  and on a pointer that does not resolve; every cell crate is audited
  (`U` means unaudited, never evidence of safety).
- **A precondition on data that crossed a trust boundary is not a
  precondition — it is a check you owe.** If a value can reach a function
  from a client, a peer, or a file, do not document "callers must not pass
  X" and `debug_assert!` it: `debug_assert!` is absent from
  `[profile.release]`, so the shipping build has no check at all, and a
  debug build turns the same input into a cell panic. Enforce it where it
  can be enforced — normalize, bound, or return a typed error — and turn
  the assertion into a **postcondition on what you emit**. For example, a
  RESP line writer must enforce framing even when a caller interpolates
  client bytes into an error message (ADR-0097).
- **Preserve the meaning of failure.** Keep the original error category
  and add bounded diagnostic context. Never turn corrupt storage into
  "not found", a failed write into success, or an ambiguous timeout into
  permission to repeat a non-idempotent operation.
- fsync failure is fail-stop under the durability contract. Other
  unrecoverable failures follow their explicit fail-stop policy; never
  invent a fallback that silently weakens acknowledged durability.

### Publication and recovery

- **Name the publication point.** Specify when a mutation becomes visible,
  what an acknowledgment promises, and which failures may still occur.
  Validate and reserve first; publish through the owning seam. A typed
  error after partial mutation is not atomic failure handling.
- Keep written, durable, and applied progress distinct and scoped to the
  correct owner and incarnation. Advance a prefix only when every required
  predecessor is covered. A later completion or larger sequence number
  alone is not proof that earlier work completed.
- Treat recovery and replay as first-class execution paths. They must
  establish the same logical invariants as live execution, using the
  persisted contract rather than new runtime defaults. Test replacement,
  deletion, namespace recreation, and interruption at publication boundaries.
- A checksum checks byte integrity, not record identity, placement,
  ordering, or semantic validity. Validate those independently. Unknown
  formats and malformed committed data take the contract's refusal path;
  never guess, skip, or repair away acknowledged state.
- Crash safety needs an explicit write/sync/rename/directory-sync order
  where applicable. Resource reclamation needs proof that all required
  readers, snapshots, and recovery paths have released the old version.
  Test cuts between steps; success on an orderly shutdown is insufficient.

### Unsafe Rust

Safe Rust is the default; `#![forbid(unsafe_code)]` everywhere except the
audited leaf crates (`inf-simd`, `inf-alloc`, `inf-fabric`, `inf-runtime`'s
backend/affinity/executor modules) and the module-scoped regions
`inf_doc::emit`, `inf_server::log_bytes`, `inf_probe::evict`,
`inf_sim::{net, steel}` (ADR-0049, ADR-0121 — the crate stays
`deny(unsafe_code)` at its root with `#[allow(unsafe_code)]` on exactly
the audited `mod` items, or one whole-file inner allow; never on a
function or block). The posture is mechanical: `check-unsafe-roots.sh`
refuses a crate root with no attribute, a `deny` root outside the master
plan's audited set, a listed leaf that went `forbid`, and any allow that
is not module-scoped — a new unsafe block outside a named module is a
compile error in every build.
Every unsafe block has a concrete `// SAFETY:` argument, an
entry in the crate's `SAFETY.md` inventory (script-checked), Miri/Loom
coverage where applicable, and a reviewer who read the argument, not just
the code. Target: < 2% of LoC. If you can express it safely at equal
measured cost, the unsafe version is wrong.

- Every `unsafe fn` documents caller obligations in `# Safety`. Explain
  alignment, initialization, bounds, aliasing, provenance, lifetime,
  pinning, and thread ownership where relevant. "The caller checked" must
  name a check the safe API actually enforces.
- Keep unsafe regions small and safe interfaces narrow. A borrowed view
  must not outlive its backing storage. Address stability must come from
  the allocation and lifetime contract, with `Pin` where needed. Passing
  a reference does not promise address stability after the borrow ends.
- Prove when the kernel or another cell has finished with a buffer before
  reuse. Cancellation requested is not terminal completion. Generation
  tokens reject stale completions; they do not substitute for ownership
  or memory-ordering proofs.
- Miri checks the supported memory-safety paths; Loom checks the modeled
  synchronization. Neither proves an unmodeled kernel, device, or protocol
  contract. Name those boundaries and the separate tests that exercise them.

### Tooling as safety

All compiler and clippy warnings are errors from day one (`-D warnings`).
The mechanical checks — `check-dep-dag.sh` (crate boundaries),
`check-cell-denylist.sh` (no locks/sleep/ambient time in cells) with
`check-clock-ban.sh` (the type-resolved half of the same rule: clippy's
`disallowed-methods` for `Instant`/`SystemTime` `now`/`elapsed`, libc
and the TSC, proven on a planted-bypass probe — ADR-0106 D7; an entry
the lint cannot resolve is a violation, not a warning),
fault-point and fsync-fail-stop greps, the attribution-divergence gate,
`check-shipping-features.sh` (no test/DST feature on a normal dependency
edge — ADR-0107 D1), `check-release-asserts.sh` (the classified
release-assert inventory — ADR-0107 D2), `check-unsafe-roots.sh` (every
crate root governs `unsafe_code`, the deny set is the leaf list,
allows are module-scoped — ADR-0121) —
are not bureaucracy; they are laws made cheap. Never weaken a check to
merge; change the law first (ADR) or fix the code.

A check that silently checks nothing gives false confidence. Every
`check-*.sh` **asserts its scope** (ADR-0106): a missing directory, an empty
file set, or a truncated scan is a failure, not a skip; the success line
discloses what was scanned;
exemptions are per-site markers that carry a reason (`denylist-allow:
<why>`, `panic-policy-allow: <why>`, and for the clippy-resolved bans
`#[allow(clippy::disallowed_methods, reason = "<why>")]` on the
statement or function — never on a crate or file inside cell code) and
are listed in the output; and
`check-scripts-selftest.sh` runs a planted violation through each gate
inside `just check` — a gate that cannot go red is not a gate. The same
rule binds CI evidence: a workflow never writes a number an instrument
should have measured (the nightly's `sim_seconds=` lines come from the
virtual clocks that advanced), and a sweep recipe's exit status is the
verdict of every shard.

Documentation identities are checked too (ADR-0106 D15):
`check-doc-artifacts.sh` enforces unique ADR numbers and the link to the
one generated compatibility matrix when the parent governance checkout
is present. D17 also checks local Markdown links in this document,
`ARCHITECTURE.md`, the master plan and the execution plans, numbered ADR
paths, obsolete layout names and landed-ADR placeholders. Abbreviated module
names and future deliverables remain review obligations. Standalone
workspace CI explicitly reports absent parent documents and unvalidated
parent links. The release job checks the matrix against its renderer before
packaging it. The dependency gate (D16) checks active and reserved edges in
both directions, requires a row for every package and prints dev exemptions.

## Performance

Design for performance before implementation, then measure the result.
Resource budgets guide design; they are not achieved numbers. The goal is
useful throughput within latency, memory, and durability contracts.

### First-principles budgets

- **Napkin math before code.** The best time for the 1000× win is design
  time. Sketch **network, disk, memory, and CPU**, each in latency and
  bandwidth. Estimate bytes moved, dependent accesses, cycles, syscalls,
  fabric hops, and durability barriers per operation and per batch.
- **Weight cost by frequency.** Start with the largest contribution to the
  operation's critical path or resource demand. Network and disk often
  dominate, but repeated memory misses or CPU work can cost more. Name the
  bottleneck and the observation that would disprove it.
- Budget both **per operation and per byte**. Short-key GET, large-value
  SET, a cold lookup, and an expanded reply stress different resources.
  Include occupancy, concurrency, skew, and background work in the model.
  A hot key's owning cell can saturate while node-wide CPU appears idle.
- Distinguish latency from throughput and service time from queueing time.
  Derive queue capacity and bytes retained from the admitted work and wait
  bound. More in-flight requests cannot create device or CPU capacity.
- Include construction, resize, invalidation, cancellation, teardown, and
  recovery in complexity analysis. An amortized O(1) operation can still
  contain an O(N) pause. The worst legal input belongs in the design budget.

### CPU predictability

- **Give the CPU coherent runs of work.** Keep hot loops compact and their
  access patterns regular. Hoist invariant decisions out of loops; select
  backend, CPU kernel, and stable configuration at their established
  boundaries. Process compatible work together only where ordering and
  fairness permit it.
- **Keep the common path direct.** Separate rare errors, diagnostics, and
  slow paths from repeated work. Avoid redundant parsing, hashing, bounds
  calculations, and mode tests. Branchless code, branch hints, and forced
  inlining are hypotheses requiring evidence, not default improvements.
- **Expose the data a loop needs.** Prefer slices, scalar parameters, or
  narrow borrows to passing a large mutable context. Keep repeated field
  loads and loop dependencies apparent. A method is fine when it gives the
  compiler and reviewer the same clear information; removing `&self` is
  not itself an optimization.
- Use static dispatch in hot loops and the registered command/engine seams
  at their boundaries. Repeated dynamic dispatch, hidden clones, and
  incidental loop-local allocation require a named acceptance criterion
  and an end-to-end A/B. Generics also cost instruction-cache space:
  specialize only where the benefit justifies code growth.
- **Shorten dependency chains.** Contiguous traversal and independent
  operations can expose memory-level parallelism. Pointer chasing and
  serial lookups can prevent it even with few instructions. Prefetch only
  valid addresses under the ownership contract, and measure distance,
  wasted bandwidth, and cache pollution.
- SIMD needs runtime feature detection, a supported fallback, and correct
  tails and alignment. Never read outside initialized, accessible storage
  to simplify a vector loop. Inspect generated code on the supported
  target when a win depends on vectorization, bounds-check elimination,
  copy elimination, or register reuse.

### Memory, caches, and locality

- **Memory is the product (L5).** Every allocation belongs to a named,
  counted domain; `sum(domains)` vs RSS divergence > 10% fails CI. Measure
  payloads, indexes, metadata, allocator slack, buffers, connection state,
  and retained work. Bytes per key/document/entry are release gates.
- Fit the working set to the work: compact records, contiguous storage,
  and separation of hot fields from cold metadata. Compare array-of-structs
  and struct-of-arrays against the actual access pattern. Alignment and
  padding are paid per entry; a cache-line-sized object is not automatically
  cache-efficient.
- Keep allocation and mutation near the owning cell. Account for NUMA
  placement, page faults, TLB pressure, and memory bandwidth when scaling.
  Avoid false sharing at sanctioned cross-cell boundaries; padding and
  stronger atomics require a demonstrated need. Do not introduce shared
  mutable data-plane state to save a local copy (L1).
- **Count every copy and every retained byte.** Zero-copy can pin a large
  buffer for a small reply; a bounded copy can release it sooner. Compare
  total bytes moved, peak live memory, lifetime complexity, and tail latency
  before choosing either. A cheap CPU copy can still be a memory-bandwidth
  bottleneck when multiplied by the workload.

### Batching, fairness, and tail latency

- **Batch every boundary (L3).** Syscalls, fabric hops, fsyncs, prefetches,
  and reply flushes are amortized across work. `sqes/submit`,
  `cmds/iteration`, and grouping ratios are always-on tripwires; a claim
  cannot cite a run with red tripwires.
- Bound a batch by item count, bytes, work, and flush deadline as
  appropriate. Measure both sparse traffic and saturation: waiting for a
  full batch must not strand a lone request, and a full queue must not
  starve timers, replies, or durability progress.
- **Separate control plane from data plane.** Control decides; data flows.
  Validate configuration and construct execution policy outside hot loops.
  Cold code may pay for a thorough check that protects cheap repeated work;
  it still needs resource bounds and must not block a cell.
- Background expiry, eviction, checkpoints, compaction, and reclamation
  spend explicit CPU and device budgets. Budget the expensive inner work,
  not merely the outer item count. Define resumable progress and fairness
  so a stream of small requests cannot prevent necessary maintenance.
- Slow readers and saturated peers retain memory and delay progress. Test
  those paths with bounded output, in-flight work, and cancellation. An
  optimization that moves cost into an unmeasured queue has not removed it.

### Measurement and acceptance

- **Mechanical sympathy is measured (L4).** State the hypothesis, workload,
  baseline, target metric, and regression limits before changing code. Make
  one attributable change; run the end-to-end A/B on the designated box.
  SIMD, prefetch, zero-copy, layout tricks, and allocator changes follow
  the same rule. An unproven optimization stays behind a feature flag;
  the flag never relaxes correctness requirements.
- Report throughput together with p50/p99/p99.9 latency, memory, errors,
  refusals, and batching. Cover the regimes the change affects: key/value
  size distribution, uniform/skewed access, local/remote routing,
  hot/cold data, durability, concurrency, and pressure. Do not improve a
  percentile by silently dropping work or excluding stalls from timing.
- Apply utilization, saturation, and errors (USE) to both server and load
  generator. Profiles and CPU counters explain deltas: cycles, instructions,
  branch misses, cache/TLB misses, and stalled or off-CPU time. A better
  counter alone does not establish a better database.
- Record revision and dirty state, build features, toolchain, workload,
  affinity, SMT, governor/EPP, thermals, device configuration, and results
  with their spread. Keep raw output local and ignored. Public claims require
  exact reproduction commands, baseline revisions, the clean-tree reference
  tier and 3–5 replicates under the evidence policy. Never present a noisy or saturated
  generator's result as server capacity.
- Record `Accepted`, `Rejected`, or `Revised` with evidence. A losing A/B
  is useful evidence; keep the result and do not merge the losing
  optimization. Correctness fixes may be `Correctness-only`; unrun
  measurements remain `Evidence-pending`, with the missing run named.
- `inf-bench` proves in-house gates. Disclose a workload the external generator cannot drive; 
  never silently substitute instruments. Only the claim ledger authorizes public numbers (L10).

## Developer Experience

Code should make the correct change easy to find, implement, and verify.
Naming, cache invalidation, and off-by-one errors deserve explicit rules:
they are recurring sources of database defects, not just readability issues.

### Naming things

- Follow Rust conventions (`snake_case` functions, variables, and modules;
  `SCREAMING_SNAKE_CASE` constants; `UpperCamelCase` types;
  acronyms as words: `Crc32Frame`, not `CRC32Frame`), then our additions.
- **Get the nouns and verbs right.** A name that requires its own
  explanation is a draft. Prefer nouns that survive being spoken in a
  design review and written in a ledger (`replica.pipeline`, not
  `replica.preparing`).
- **Units and qualifiers go last, most significant first**:
  `latency_ms_max`, `budget_bytes_slice`, `expiry_fires_per_slice_cap`.
  Related names line up and sort together.
- **No abbreviations** in identifiers (loop counters and established domain
  terms excepted). The allowlist (ADR-0125 A5): `lsn`, `crc`, `ttl`, `ns`
  for namespace; `buf` and `cfg` (the standard library's and Cargo's own
  spellings); `ctx`; `ckpt` (the checkpoint format's name); `ptr` inside
  the unsafe leaves; `idx` and `prev`/`next` as loop-local names. Anything
  else is renamed when its function is next touched — there is no naming
  gate. Long-form flags in scripts and CLIs: `--reference-box`, never `-r`.
- Prefer clear, symmetric pairs such as `source`/`target` and
  `begin`/`end`. `src`/`dst`/`dest` are not used (renamed workspace-wide
  in batch 68). Meaning takes precedence over matching name lengths.
- Infuse allocator/handle names with their contract: `arena:`-prefixed
  things do not get freed item-by-item; `pool` things return whence they
  came; a `lease` must be returned before suspension.
- Don't overload a word with two meanings. A cell is the execution owner;
  `shard` names only the on-disk per-cell directory family (`shard-N/`,
  `shard_dir`) and nothing else;
  a partition is a unit of data ownership. Likewise, submitted, written,
  durable, and applied describe distinct progress states. Use the vocabulary
  of the owning contract consistently in code, metrics, and documentation.
- Order matters: module purpose and central types before implementation
  details; public API before its private helpers. Group by responsibility
  so the file reads top-down. Keep visibility as narrow as possible.
- Make borrowing, ownership transfer, and fallibility apparent at the call
  site. Specify options that affect durability, limits, ordering, or format
  compatibility explicitly; library defaults are not our contract.

### Comments and commits

- **Always say why.** Comments carry the reasoning the code cannot:
  constraints, rejected alternatives, the invariant being protected, the
  reproducible benchmark and decision that justified the trick. Comments
  that narrate what the next line does are noise; delete them.
- Keep comments concise: a complete sentence for a rationale or invariant,
  a short phrase for an obvious inline label. Explain a nontrivial test's
  trigger, oracle, and expected failure; do not narrate each assertion.
- Use a short, one-line commit message stating the concrete change. Keep
  detailed reasoning, reproduction commands and results in checked-in
  documentation and the PR description. Do not append model co-author trailers.

### Cache invalidation

- **One authoritative fact; explicit derived state.** Before adding a cache,
  decide whether recomputation is cheaper and simpler. A cache needs an
  owner, capacity bound, lookup key, validity rule, and reclamation path.
  Explain the measured benefit and the extra memory it retains.
- Include every dependency in validity: key and namespace identity,
  incarnation or generation, revision, schema, and relevant options.
  Cached absence needs invalidation too. A TTL bounds staleness only when
  the API permits staleness; it does not establish coherence.
- Put invalidation at the owning mutation boundary. Enumerate the paths
  that can change the cached fact: replacement, deletion, expiry, eviction,
  rename, flush, namespace drop/recreation, rebuild, and configuration
  changes as applicable. One command's invalidation hook cannot cover
  another path that bypasses it.
- Revalidate after suspension, callback, or deferred completion. An index
  slot, pointer, or integer ID can name a different object after reuse;
  validate its generation and the fact being used. Copying a handle does
  not preserve the freshness of the observation that produced it.
- A hash, fingerprint, or checksum is not exact identity. Verify full keys
  and the required generation before overwrite, deletion, accounting, or
  publication. A stale prefetch hint may affect speed; it must not decide
  which bytes belong to a key.
- Test cache hits and misses against the authoritative state through
  mutation and recovery. Include negative entries, delete/recreate, stale
  replies, and resume-after-invalidation cases, not just steady reads.

### Off-by-one errors

Use distinct names and types for different quantities:

| Quantity | Meaning | Boundary rule |
|----------|---------|---------------|
| `index` | Position of an existing element | `index < count` |
| `count` | Number of elements; may be zero | Last index exists only when `count > 0` |
| `offset_bytes` | Byte position in a buffer or file | Validate against the correct storage extent |
| `length_bytes` | Number of bytes in a span | Prove the whole span fits |
| `end_exclusive` | First position outside a range | `begin <= end_exclusive <= length` |

- Prefer half-open ranges `[begin, end)`. Empty ranges are valid where the
  contract permits them. For a nonempty prefix, the last index is
  `count - 1`; `index + 1` counts elements through that index, not the
  entire collection. Check overflow on conversions between quantities.
- Validate slices without overflow: establish `offset <= buffer_len`,
  then `length <= buffer_len - offset`, or use `checked_add` and reject
  failure before slicing. A successful narrow cast is not a bounds check.
- Show division and rounding intent: floor, ceiling, or exact division.
  Reject a zero divisor, check exact divisibility when required, and prove
  that alignment rounding and `count * element_size` are representable.
  Reuse checked helpers rather than open-coding overflow-prone rounding.
- Define inclusive/exclusive semantics for deadlines, TTL expiry, cursor
  continuation, sequence numbers, and watermarks. A storage offset may
  identify an entry's start, end, or next position; the type and contract
  must say which. Do not mix units or ordering scopes in comparisons.
- Exercise zero, one, limit minus one, limit, and limit plus one where
  representable; include the integer maximum. Test empty and full buffers,
  exact page/frame boundaries, partial final chunks, and wrap/reuse paths.

### Scope, aliasing, and time

- Declare variables at the **smallest possible scope**; compute values
  **where they are used**, not paragraphs earlier — every line between
  check and use is room for drift (POCPOU).
- Keep mutable access narrow and one owner per authoritative fact (L1).
  A local cached value needs the same freshness reasoning as a larger cache.
  Do not keep a borrow alive merely to avoid a cheap recomputation.
- Derive `Copy` only when implicit copying is cheap and semantically safe.
  Review large by-value arguments and returns for stack use and copy cost;
  cloning an owned structure to satisfy the borrow checker can hide both
  an ownership problem and a latency spike.
- Zero every padding byte that crosses a trust boundary (wire buffers, log
  frames). Initialize every transmitted byte and serialize only the valid
  length, never spare capacity or raw Rust struct padding. A checksum can
  faithfully protect accidentally emitted stale bytes; it cannot prove
  that the encoder meant to send them.
- Time, randomness, disk, network, and fabric effects are **injected**
  (L7). Ambient `Instant::now()` or `rand::random()` in cell code is a
  denylist violation — and for clocks a clippy error under any spelling
  (`disallowed-methods`, ADR-0106 D7; `UNIX_EPOCH.elapsed()` and
  `_rdtsc` included): it breaks the simulator's authority over the
  universe, which is the single most valuable testing asset we own.
  The same rule reaches containers: a cell-resident `HashMap`/`HashSet`
  never carries `std`'s per-process `RandomState` — use
  `inf_foundation::BuildIntHasher` for internally generated keys
  (addresses, tokens, keyed-hash outputs) or a `BTreeMap` when ordered
  traversal is required. User-controlled keys use the keyed-hash contract;
  a deterministic integer hasher is not a substitute. Hash-map iteration
  order must not choose state-changing execution order.
- Distinguish injected monotonic time for waits from persisted wall-clock
  deadlines. Define equality at expiry and behavior across restart or a
  clock jump. Avoid tests that depend on a real sleep to establish order.

### Tests and review

- Keep tests, fixtures, seeds and harnesses in source. Generated logs,
  profiles and reports stay ignored; record commands and results, not output
  archives. See [validation and reference hardware](validation.md).
- **Red first for a defect.** Keep the smallest failing reproducer, then
  test the fix at the layer that owns the invariant. If clients can reach
  the bug, retain a client/binary test as well. Show that the intended
  assertion or oracle catches the pre-fix behavior.
- Test contracts and observable outcomes, not a copy of the implementation.
  Use an independent model, byte-exact compatibility oracle, or separately
  derived invariant. Include invalid inputs, refusal, partial I/O,
  cancellation, reordered completion, and crash/restart where relevant.
- Keep failing seeds, minimized inputs, exact commands, build features,
  and expected outcomes. A simulator pass proves only the modeled paths;
  distinguish model-only, runtime, and real-reference evidence. A green
  test that never reached the relevant fault point closes nothing.
- Use unit/property tests for local contracts, fuzzing for decoders, Miri
  for unsafe memory, Loom for synchronization, and DST/crash tests for
  histories. Verify the shipping feature set and slim builds when affected;
  test-only features must not alter the production contract.
- Review the failure and cleanup paths with the happy path. For semantic
  refactors, preserve an equivalence oracle that can detect changed output,
  ordering, or errors. State what ran, what did not, and why; a missing tool
  or skipped campaign is not a passing result.

### Style by the numbers

- `cargo fmt` settles formatting arguments: edition 2024, `max_width = 100`
  (hard limit — nothing hides past a horizontal scrollbar), Unix newlines,
  4-space indentation. Toolchain pinned (`rust-toolchain.toml`); MSRV moves
  by decision, not drift.
- Keep Rust's explicit braced control flow. Let rustfmt format short
  expressions; do not compress several state transitions onto one line.
- One hundred columns fits two files side by side. Use the width; never
  exceed it. `cargo fmt --check` cannot see a long string literal, comment,
  attribute or macro body (rustfmt leaves them alone), so
  `scripts/check-line-width.sh` counts every line of every Rust file
  (ADR-0125).

## Dependencies

Dependencies are **few and deliberate**. Keep core data-plane crates
effectively free of third-party dependencies; intentional edge dependencies
have an ADR. The dependency permission map, `docs/dep-dag.toml`, is the
authority for allowed edges; do not bypass it through a re-export.
checked for all dependency kinds by `check-dep-dag.sh` (ADR-0025,
ADR-0106 D16). The in-house gate instrument `inf-bench` deliberately uses
`inf-foundation` and the tooling TOML parser.

Review a dependency's transitive graph, features, unsafe code, allocation,
blocking behavior, determinism, portability, build cost, and maintenance.
`cargo deny check` gates licenses and advisories. Pin reproducible inputs
and review updates as code changes. Disable unused features and keep
optional engines absent from slim builds. A familiar API is not a reason
to import a runtime or allocator into a cell.

## Tools

A small standardized toolbox beats an array of specialized instruments:
`cargo`, `just`, and the checked-in scripts are the interface; if a task
needs a new tool, prefer a small Rust binary under `bins/` over a shell
script that works on exactly one machine. Keep shell for straightforward
process composition; move complex state and parsing into typed, tested code.

- Make the documented command the real entry point. Run from the workspace
  root; use `just check` and `cargo deny check` for the required baseline,
  then the affected layer's validation. Keep local and CI invocations aligned.
- Scripts validate arguments, quote paths, use explicit working directories,
  preserve command failures through pipelines, and collect every child
  process's exit status. Give subprocesses a bounded lifetime and cleanup.
- Tests and tools use isolated temporary directories and explicit targets.
  Do not assume a fixed port, an existing server, or a developer's data
  directory. Make seeds, configuration, and artifact paths reproducible.
- Diagnostics identify the operation, relevant bounded identifiers, and
  failure reason. Do not dump arbitrary values or unbounded buffers into
  logs. A failure should provide enough context to reproduce it without
  making the failure path another source of resource exhaustion.

## The Last Stage

Keep revising until the design is simple enough to explain, the failure
paths are explicit, and the evidence matches the claim. When a rule needs
to change, give the reason and update its governing document; change a
frozen contract by ADR before implementation. Leave the next engineer a
smaller problem and a stronger proof.
