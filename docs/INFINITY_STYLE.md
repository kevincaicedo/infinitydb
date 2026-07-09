# InfinityStyle

> Heavily inspired by — and openly indebted to — TigerBeetle's
> [TIGER_STYLE](https://github.com/tigerbeetle/tigerbeetle/blob/main/docs/TIGER_STYLE.md),
> adapted to InfinityDB's language (Rust), laws (L1–L11 in
> `docs/infinity-master-plan.md`), and evidence discipline (§18–§19). Where
> this document and the master plan conflict, the master plan wins. Where
> this document is stricter than your habits, this document wins.
>
> Normative for all code under `infinitydb/`. Reviewers affirm conformance
> per the milestone execution-discipline sections (ADR-0025).

## Why Have Style?

Another word for style is design. Our design goals are **safety,
performance, and developer experience — in that order**. All three matter;
the order settles arguments. Style is not readability cosmetics: it is the
set of decisions that make the next thousand decisions cheaper and safer.

InfinityDB exists because its predecessor, Vortex, was fast in pieces and
wrong as a system (master plan §2). Every rule below traces to a law, a
post-mortem root cause, or a measured incident. When you find a rule without
a reason, fix the document — never obey mystery rules, and never break
reasoned ones casually.

## Simplicity and Technical Debt

Simplicity is the hardest revision, not the first draft. We spend design
energy up front — sketches, budgets, interface freezes — because an hour of
design is worth weeks in production, and because our STOP-gate discipline
means a wrong foundation halts the whole train (RC-5).

**Zero technical debt, or visible debt.** We do it right the first time,
or we write the debt down where it cannot hide: a claim-ledger row, a
`Proposed (needs ADR)` marker, an `Evidence-pending` disposition, an
explicit debt-forward entry in the milestone plan. The one unforgivable
form of debt is the silent kind. A known bug we ship is a documented
limitation; an unknown bug we could have caught with an assertion is a
process failure.

## Safety

The [NASA Power of Ten](https://spinroot.com/gerard/pdf/P10.pdf) rules,
translated to our world:

### Control flow

- **Simple, explicit control flow only.** Prefer `?`, `let-else`, early
  returns, and small helpers over nested pyramids. Push `if`s up and `for`s
  down: parents own branching and state; leaf functions are pure and
  branch-poor.
- **No recursion in decoders or on the data plane.** Every parser (RESP,
  JSON, JSONPath, PartiQL, log frames, cursors, fabric codec) is iterative
  with an explicit stack and an explicit depth limit, and its fuzz target
  lands **in the same PR** (L9). Recursion elsewhere needs a proven bound
  and a reviewer who agrees.
- **Hard limit: ~70 lines per function.** If you scroll, you split. The
  right split keeps control flow in the parent and moves straight-line work
  to helpers — never the reverse.
- **Every command is a resumable state machine (L6).** Suspension points are
  few, typed, and visible. Never hold buffer leases, response iovecs, ring
  slots, arena borrows, staged log records, or command-local guards across a
  suspension, requeue, or reactor-iteration boundary — this is our
  place-of-check-to-place-of-use rule, and it is checked in review every
  time.
- **Don't react to external events directly; run at your own pace.** The
  reactor reaps, drains, parses, executes, and submits in **batches** under
  budgets (L3). Code that does per-event work at a boundary — one syscall
  per op, one wake per message, one fsync per write — is architecturally
  wrong even when it benchmarks fine at low load.

### Put a limit on everything

Everything has a limit in reality; honest code states it. Every loop bounds
its iterations or is an asserted-infinite event loop. Every queue, ring,
cache, page, batch, and backlog has a fixed cap. Backpressure is credits,
budgets, output caps, and admission refusal — **never** an unbounded queue
standing in for flow control. When you bound coverage (top-N, sampling,
drain caps), the bound is visible in metrics and reports — silent truncation
reads as completeness and is therefore a lie (L10).

### Types

- **Make invalid states unrepresentable**: newtypes for ids and offsets,
  enums for state machines, typestate for phase-ordered resources (the
  buffer/token lifecycles), generation tokens where ABA lurks. A bug the
  type system catches costs nothing forever.
- **Explicitly sized integers** (`u32`, `u64`, `i64`) in every wire format,
  log record, index slot, and counter. `usize` is for in-memory slice
  indexing only — it never crosses a serialization boundary.
- Distinguish `index`, `count`, and `size` in names and in casts — the
  off-by-one trio. Show division intent: `div_ceil`, explicit floor,
  or a comment proving exactness.
- Simple signatures and return types. As a return type, `()` beats `bool`,
  `bool` beats `u64`, `u64` beats `Option<u64>`, `Option` beats `Result` —
  every step down that ladder multiplies call-site branches.

### Assertions

Assertions detect programmer errors; operating errors get handled, never
asserted. The only correct response to corrupt code is to crash — assertions
downgrade catastrophic correctness bugs into liveness bugs, and they are a
force multiplier for DST and fuzzing.

- **Assert arguments, return values, preconditions, postconditions, and
  invariants.** Target an average of two assertions per function on the
  data plane. `debug_assert!` is the default; promotion to a release
  `assert!` is a deliberate act for invariants whose violation endangers
  durable state, and carries a ≤ 1% A/B (the M2.5-S13 rule).
- **Pair assertions**: enforce a property at two code paths (assert before
  writing to disk *and* after reading back; before fabric enqueue *and*
  after dequeue). Data crossing a boundary is where the interesting bugs
  live — assert the positive space you expect *and* the negative space you
  forbid.
- **Split compound assertions** — `assert!(a); assert!(b);` reads better and
  fails more precisely than `assert!(a && b)`.
- **Assert compile-time relationships** with const assertions (record
  header sizes, ring slot alignment, enum discriminant ranges). A design
  error caught before the program runs is the cheapest bug of all.
- Every state machine keeps a written **invariant inventory** (what holds,
  how it is enforced, what is deliberately unchecked) — interfaces docs
  appendix, kept current by the story that changes the machine.
- Assertions are a safety net, not a substitute for understanding. Build
  the mental model first, encode it in assertions, explain it in comments,
  and let the simulator hunt what both of you missed.

### Panics and errors

- **Panics are for violated internal invariants only.** Input validation,
  protocol errors, I/O failures, allocation pressure, disk-full, and every
  other operating condition returns a typed error. The CI panic-policy grep
  enforces the letter; you enforce the spirit.
- **All errors are handled.** The majority of catastrophic production
  failures in distributed systems come from mishandled *non-fatal* errors —
  so error paths get tests, fault points, and crash-matrix rows like any
  other code. `unwrap()`/`expect()` on an operational `Result` is a review
  reject; `expect()` with an invariant justification is an assertion and is
  judged as one.
- fsync failure is fail-stop. Never silently degrade durability (§8.4).

### Unsafe Rust

Safe Rust is the default; `#![forbid(unsafe_code)]` everywhere except the
audited leaf crates (`inf-simd`, `inf-alloc`, `inf-fabric`, parts of
`inf-runtime`). Every unsafe block has a concrete `// SAFETY:` argument, an
entry in the crate's `SAFETY.md` inventory (script-checked), Miri/Loom
coverage where applicable, and a reviewer who read the argument, not just
the code. Target: < 2% of LoC. If you can express it safely at equal
measured cost, the unsafe version is wrong.

### Tooling as safety

All compiler and clippy warnings are errors from day one (`-D warnings`).
The mechanical checks — `check-dep-dag.sh` (crate boundaries),
`check-cell-denylist.sh` (no locks/sleep/ambient time in cells),
fault-point and fsync-fail-stop greps, the attribution-divergence gate —
are not bureaucracy; they are laws made cheap. Never weaken a check to
merge; change the law first (ADR) or fix the code.

## Performance

> "The lack of back-of-the-envelope performance sketches is the root of all
> evil." — attributed with love, via TigerBeetle.

- **Napkin math before code.** The best time for the 1000× win is design
  time, when profilers cannot help you. Sketch the four resources —
  network, disk, memory, CPU — in both bandwidth and latency, and land
  within an order of magnitude before writing a line. Master plan §18.1 is
  the house example: a cycle budget per pipelined GET that the
  implementation is *accounted against* (M2.5-S09), not a hope.
- **Optimize the slowest resource first** (network → disk → memory → CPU),
  weighted by frequency: a cache miss paid a million times outruns an fsync
  paid once.
- **Batch every boundary (L3).** Syscalls, fabric hops, fsyncs, prefetches,
  and reply flushes are paid per batch. `sqes/submit`, `cmds/iteration`,
  and the grouping ratio are always-on tripwires with gates — a benchmark
  from a run with red tripwires does not exist (§19).
- **Separate control plane from data plane.** Control decides; data flows.
  Keep control-plane `if`s out of data-plane `for`s; give the CPU long
  predictable runs (the sprinter, not the parkour artist). Control-plane
  code may spend O(N) on assertions to protect an O(1) decision.
- **Be explicit; don't lean on the optimizer.** Extract hot loops into
  standalone functions with primitive arguments (no `&self` the compiler
  must reason about). Static dispatch and monomorphized generics on the hot
  path; `dyn` in cold code or behind an A/B artifact. No hidden clones, no
  loop-local allocation, no global-allocator dependence on the data plane.
- **Mechanical sympathy is measured, not assumed (L4).** SIMD, prefetch,
  zero-copy, layout tricks, new allocators: hypothesis → end-to-end A/B on
  the designated box → `Accepted`/`Rejected`/`Revised` disposition with the
  artifact. **A losing A/B is a successful experiment**: record the finding,
  do not merge the code (the M0-S14 rule). Profilers explain deltas; they
  do not replace them.
- **Memory is the product (L5).** Every allocation belongs to a named,
  counted domain; `sum(domains)` vs RSS divergence > 10% fails CI. Bytes
  per key/document/entry are release gates, not dashboards.

## Developer Experience

### Naming things

- Follow Rust conventions (`snake_case` items, `UpperCamelCase` types,
  acronyms as words: `Crc32Frame`, not `CRC32Frame`), then our additions.
- **Get the nouns and verbs right.** A name that requires its own
  explanation is a draft. Prefer nouns that survive being spoken in a
  design review and written in a ledger (`replica.pipeline`, not
  `replica.preparing`).
- **Units and qualifiers go last, most significant first**:
  `latency_ms_max`, `budget_bytes_slice`, `expiry_fires_per_slice_cap`.
  Related names line up and sort together.
- **No abbreviations** in identifiers (loop counters and established domain
  terms — `lsn`, `crc`, `ttl`, `ns` for namespace — excepted). Long-form
  flags in scripts and CLIs: `--reference-box`, never `-r`.
- Choose same-length pairs where symmetry helps the eye: `source`/`target`
  over `src`/`dest`.
- Infuse allocator/handle names with their contract: `arena:`-prefixed
  things do not get freed item-by-item; `pool` things return whence they
  came; a `lease` must be returned before suspension.
- Don't overload a word with two meanings (we retired "shard" vs "cell"
  ambiguity once — a cell is the thread+state unit; keep it that way).
- Order matters: `pub` API first, then internals; fields, then types, then
  methods. The file reads top-down like the design doc it secretly is.

### Comments and commits

- **Always say why.** Comments carry the reasoning the code cannot:
  constraints, rejected alternatives, the invariant being protected, the
  artifact that justified the trick (`// A/B: .artifacts/m2/...`). Comments
  that narrate what the next line does are noise; delete them.
- Comments are prose: capitalized sentences with full stops. A test starts
  with a sentence stating its goal and method, so a reader can skip or dive
  deliberately.
- Commit messages are read — by reviewers, by `git blame`, by the ledger
  auditor. Say what changed and why, name the story ID, name the artifact
  paths for evidence-bearing changes. A PR description is not stored in the
  repository and is therefore not a substitute.

### Scope, aliasing, and time

- Declare variables at the **smallest possible scope**; compute values
  **where they are used**, not paragraphs earlier — every line between
  check and use is room for drift (POCPOU).
- Don't duplicate state or alias mutable data; one owner per fact (L1 is
  this rule at architecture scale — apply it at function scale too).
- Zero every padding byte that crosses a trust boundary (wire buffers, log
  frames): buffer *underflow* — stale bytes leaking through unwritten
  padding — is the quiet sibling of overflow, and CRCs do not catch what
  was "validly" written.
- Time, randomness, disk, network, and fabric effects are **injected**
  (L7). Ambient `Instant::now()` or `rand::random()` in cell code is a
  denylist violation: it breaks the simulator's authority over the
  universe, which is the single most valuable testing asset we own.

### Style by the numbers

- `cargo fmt` settles formatting arguments: edition 2024, `max_width = 100`
  (hard limit — nothing hides past a horizontal scrollbar), Unix newlines,
  4-space indentation. Toolchain pinned (`rust-toolchain.toml`); MSRV moves
  by decision, not drift.
- Braces on every `if` unless the whole statement fits one line — defense
  in depth against `goto fail;`-class bugs.
- One hundred columns fits two files side by side. Use the width; never
  exceed it.

## Dependencies

Not zero, but **near-zero and always deliberate**. The data plane owns its
fate: core crates (`inf-foundation` through `inf-store`) carry effectively
no third-party dependencies; the deliberate exceptions live at the edges
(wasmtime for the M10 sandbox, mlua for M6 scripting — each its own ADR).
Tooling binaries (`inf-bench`, `inf-compare`) are **zero-dependency by
policy** so the measurement instrument shares no code or supply-chain
surface with the system under test. Every new dependency is a reviewed
decision (`cargo deny check` gates licenses and advisories): each one is
supply-chain risk, safety risk, compile-time cost, and a temptation to stop
understanding our own stack. The usefulness of a dependency is inversely
proportional to the lifetime of the project — and we are building for the
long term.

## Tools

A small standardized toolbox beats an array of specialized instruments:
`cargo`, `just`, and the checked-in scripts are the interface; if a task
needs a new tool, prefer a small Rust binary under `bins/` (the
`inf-compare` precedent: typed, portable, testable, zero-dep) over a shell
script that works on exactly one machine. Shell stays for what shell is
best at — five-line CI greps with no state.

## The Last Stage

These rules are seat belts, not ceremony: uncomfortable for a week,
unimaginable to drop after a month. When a rule fights the work, don't
suffer silently and don't defect silently — change the rule in the open
(this file, an ADR, the master plan) so the next person inherits your
lesson instead of your workaround.

Keep trying things, measure everything, have fun. It's called InfinityDB
not because it does everything — but because, built this way, nothing about
one machine is the limit.
