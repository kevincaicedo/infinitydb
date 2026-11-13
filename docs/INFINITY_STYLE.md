# InfinityStyle

> Inspired by TigerBeetle's
> [TIGER_STYLE](https://github.com/tigerbeetle/tigerbeetle/blob/main/docs/TIGER_STYLE.md),
> adapted to Rust and to InfinityDB's design laws.
>
> Normative for every **design and** all code in this workspace. Read it
> in full before writing a design and before writing code. The design's
> reviewer affirms conformance of the design, the code reviewer of the
> code (ADR-0025, ADR-0138). This document states enduring engineering
> rules.

## Why Have Style?

Style is design. The goals are **safety, performance, and developer
experience, in that order**. All three matter; the order settles
arguments. The unit of correctness and performance is the whole system:
a component that complicates recovery, hides memory, or stalls another
cell is an incomplete design.

**A rule is enforced by the strongest mechanism that can carry it
(L13):** a type, then a lint, then a generated table, then a self-tested
script, then review. What the compiler enforced held; what a grep, a
comment or a paragraph enforced drifted.

- A new rule lands with its enforcement, or is recorded as review-only
  with the reason.
- A rule the tree contradicts is an open finding, never precedent.
- Every rule needs a reason that a reviewer can explain and a way to
  establish conformance.

## Simplicity and Elegance

Simplicity takes design work and revision, spent before code: sketch
ownership, state transitions, failure paths and resource budgets. A
small design that brings safety, performance and clarity together is
worth more than a clever implementation of a complicated one.

- **Minimize concepts and states.** One owner, one authoritative
  representation, one path per state transition. Remove a special case
  before adding a flag that interacts with it.
- **A few strong abstractions.** An abstraction captures a domain
  invariant, narrows an API, or makes an ownership boundary explicit. No
  speculative frameworks, renaming wrappers, or generic machinery for
  capabilities we do not need.
- **Keep the proof local.** A reviewer can say why a mutation is valid,
  what it costs and what failure leaves behind without reconstructing
  distant caller assumptions. Explicit code beats a shorter expression
  that hides order, allocation or error handling.
- **Delete what you replace**: obsolete paths, flags, comments and
  configuration, in the same change. A compatibility path exists only
  for a defined contract with tests.
- **One decision, one place.** A second implementation of a contract (a
  second dispatcher, table or store) needs an ADR, not a test that keeps
  the two equal. A behavior that varies per command, format or namespace
  is a **column of one table**, and its regression test iterates the
  table. A hand-kept list that "a new X must join" is a defect waiting
  for the next X.

## Design Before Code

The cheapest defect is the one a page of design removes (L12). Work
starts only on a **written design** reviewed by someone who did not
write it. It answers, in tables and arithmetic, not prose:

1. **State and transitions**: state × event → state, effect; the
   invariant inventory, each row marked *type*, *lint*, *assert* or
   *test only*, with a reason for every row that is not *type*.
2. **Publication and failure**: the publication point, what is validated
   and reserved before it, what each failure leaves behind, what is
   durable before what is served.
3. **Scale envelope**: cost and memory at 10³, 10⁶, 10⁹ and 10¹² units,
   for steady state **and** construction, resize, teardown and recovery;
   every O(N) named with its bound per slice.
4. **Hardware budget**: per resource, operations and bytes per request,
   the queue it waits in, Little's-law depth, the resource expected to
   saturate and the counter that shows it.
5. **Limits**: every new bound with its unit, owner, crossing behavior.
6. **Adversarial inputs and the oracle**: the hostile values, the oracle
   that sees a violation, why it is independent, its canary.
7. **Where the decision lives**: the owning table or type; a second copy
   needs a reason.
8. **Falsifier**: the observation that rejects the design, and the
   control leg that separates a regression from the instrument.

- A state machine that cannot be drawn as a table is not understood yet.
- The third amendment to an ADR means the design was never drawn: stop,
  redraw it, and write a superseding ADR that states the rule as it is.
- Build the thin vertical path through every seam first; polish
  substrate after the seams have been met.

## Zero Technical Debt

Never merge a known correctness defect, a broken invariant, unbounded
work, or a shortcut that needs a later repair. Fix the root cause while
the design is understood; if the scope is too large to finish correctly,
cut it to a complete, tested contract.

- **Recording debt does not make it acceptable.** An issue, a `TODO`, a
  feature flag or an `Evidence-pending` label is no permission to ship a
  known bug. A found defect gets a preserved reproducer, a repair and a
  retained regression test before the affected work is complete. Never
  build on a known broken foundation. Existing findings stay open until
  their fixes are verified; changing this policy does not close them.
- **A boundary is not debt.** An intentionally unsupported feature with
  a defined refusal is a product boundary; an unrun benchmark is an
  evidence gap. Neither licenses a violation of promised behavior.
- **A fix answers its class question**: the table, type or lint that
  makes its sibling impossible, or that no class exists and why. Fixing
  one of five commands is an open finding with four instances left.
- **A deviation has an owner and an expiry date**; expired, it is a
  defect. "Proven at the store tier" is a substrate status: done means
  reachable from the wire at the shipped topology.
- **A plan or document sentence the tree contradicts is a red row**
  until an ADR withdraws it or a change builds it.

## Safety

The [NASA Power of Ten](https://spinroot.com/gerard/pdf/P10.pdf) and
TigerStyle inform these rules, applied to our ownership model, resumable
commands and variable-size inputs.

### Control flow

- **Simple, explicit control flow only**: `?`, `let-else`, early returns
  and small helpers, not nested pyramids. The parent keeps phase
  selection and mutation order; a leaf helper has narrow inputs and one
  job. Separate policy from repeated work; prefer pure helpers where
  they make the result easier to verify.
- **No recursion in decoders or on the data plane.** Every parser (RESP,
  JSON, JSONPath, PartiQL, log frames, cursors, fabric codec) is
  iterative, bounded and total (every input yields a value or a typed
  error); a nested grammar uses an explicit stack with a depth limit. A
  grammar whose type forbids nesting beyond one level (a fabric batch, a
  JSONPath descend) is a two-level walk, a leaf function and a caller
  that loops it, never a self-call; its encoder and printer take the
  same shape (ADR-0125 A4). Its fuzz target lands **in the same PR**
  (L9). Recursion elsewhere needs a proven bound and a reviewer who
  agrees (ADR-0125 A4).
- **Function limit: 70 code lines** (ADR-0125). Split by responsibility,
  keeping the transition visible and the helper contract meaningful.
  `scripts/check-lint-ratchet.sh` (ADR-0144 D3, the one ratchet) runs
  clippy's `too_many_lines` on every production target against the
  `fn_length` rows of `docs/lint-baseline.tsv`: a file's count never
  goes up, the row comes down with the code, and the total never exceeds
  the table's approved copies (`HEAD`, the base branch tip, its
  introducing commit), so a violation cannot arrive with its own raised
  row. The baseline records existing violations to remove; it is no
  budget for new ones, and one oversized function is never traded for
  another merely because the per-file count stays level. The only
  opt-out is `#[allow(clippy::too_many_lines, reason = "shape: …")]`,
  printed on `check-lint-scopes.sh`'s OK line.
- **Hard limit: 3000 production lines per file** (ADR-0125;
  `scripts/check-file-length.sh`). Tests do not count
  (`strip-test-modules.awk` decides, as the panic-policy gate sees the
  tree). Over the bar a file becomes a folder: behavior moves into child
  modules, `pub(super)` marks what crosses, and the parent re-exports
  every public path so callers never move.
- **Every command is a resumable state machine (L6)**, its suspension
  points few, typed and visible. Never hold a buffer lease, response
  iovec, ring slot, arena borrow, staged log record or command-local
  guard across a suspension, requeue or reactor-iteration boundary: this
  is our place-of-check-to-place-of-use rule, and it is checked in
  review every time. A single-threaded cell still interleaves across a
  yield. Keep only what the seam explicitly permits (owned bytes, typed
  identities, bounded reservations); reacquire and revalidate mutable
  facts on resume. A position, ordinal or address validated before a
  suspension is not valid after it: walkers resume through a typed
  cursor keyed by identity, never `skip(n)` or a remembered offset.
  Prefer a non-`Copy`, non-`'static` handle, so that holding it across
  an `await` is a compile error.
- **Don't react to external events directly; run at your own pace
  (L3).** The reactor reaps, drains, parses, executes and submits in
  **batches** under budgets. Per-event work at a boundary (one syscall
  per op, one wake per message, one fsync per write) is wrong even when
  it benchmarks fine at low load. A batch may hold one item; flush and
  fairness deadlines still apply.
- **Make progress explicit.** Each step consumes input, advances a
  cursor, spends a bounded budget, waits for a named event or
  terminates, and names who schedules the next. Requeueing unchanged
  work without a wakeup condition is a busy loop.

### Put a limit on everything

Every loop has a bound, or a documented event-loop lifetime with bounded
work per iteration. Every queue, ring, cache, page, batch, retry
sequence and backlog has a cap and a defined full-state behavior.
Configuration validates limits and their arithmetic before allocating or
serving.

- **Every limit is a row of `limits`: unit, owner, and what a client
  observes at the crossing** (a typed refusal, a continuation, pacing,
  or a documented fail-stop). A limit that exists only as a literal is a
  defect. Until the workspace `limits` table exists, a new bound is a
  named `const` in its crate's `limits` module with the crossing
  behavior in its doc comment.
- **Width is a limit.** A type that can be exhausted (`u32` heap
  addresses, lifetime-cumulative offsets, generation bits) states its
  exhaustion policy where it is declared.
- **One `const` owns each on-disk or wire bound**; encoder and decoder
  both consume it. A writer that can emit what the reader refuses is a
  defect at default flags.
- **A growable std container has no cap**, so in cell crates `HashMap`,
  `HashSet` and `VecDeque` are clippy `disallowed-types` (ADR-0163 D2).
  An item not yet migrated carries a `container: <record>` allow, and
  `docs/container-exemptions.tsv` counts what each allow covers: a new
  container in an exempt item is red, and the counts only shrink. Code
  outside the cell crates allows the workspace-wide lint once, with a
  reason: a test target, bench or binary at its crate root
  (`#![allow(clippy::disallowed_types, reason = "test target: …")]`), a
  test module inside a cell crate on its `#[cfg(test)] mod` (`reason =
  "test-only: …"`).
- **Bound bytes and work, not only item counts**: fan-out, response
  expansion, nesting, scratch space, bytes retained while waiting. A few
  large values can monopolize a core.
- **Backpressure is credits, budgets, output caps and the contract's
  admission or pacing policy**, never an unbounded queue or a spin
  retry. Distinguish an impossible request from one waiting for
  temporary capacity.
- **Every retry has a bound, a deadline, a cancellation and a progress
  condition.** A timeout ends waiting; it does not prove that a mutation
  did not execute.
- **At a coverage limit** return a continuation, a typed refusal or an
  explicitly partial result, as the API defines. Silent truncation never
  stands in for a complete result (L10).

### Allocation and resource ownership

Keys, values, documents and replies are variable-size, so allocation is
part of the workload: bounded, accounted ownership, without paying
worst-case capacity for every small value.

- Preallocate fixed control structures and reusable scratch where the
  capacity is known. Admitted variable-size data lives in cell-owned
  arenas, slabs and pools. Variable payloads do not excuse incidental
  allocation in hot paths. New global-allocator reliance or per-item
  scratch growth needs a named acceptance criterion and an A/B; admitted
  payload allocation through the owning domain is normal operation.
- Validate length, checked size arithmetic, representation overhead and
  quota before reserving. Attribute capacity, metadata, alignment, slack
  and retained buffers to named domains: logical value length is not the
  memory bill. Count a reservation once across its lifetime.
- **Reserve before publishing.** Know the worst-case resources a
  mutation needs before changing visible state, and handle exhaustion
  through fallible APIs. An allocation failure never exposes half a
  replacement or destroys the value being replaced.
- **Name who releases** every allocation, credit, token and handle on
  success, refusal, cancellation, disconnect and shutdown: RAII for
  synchronous cleanup, an explicit state machine for cleanup that needs
  I/O. Destructors do not establish crash durability.
- **Growth, shrinking, copying, rehashing and destruction are work**,
  bounded per slice, including the peak while old and new
  representations coexist. Amortized complexity does not bound one
  resize or one large drop.

### Types

- **Make invalid states unrepresentable**: newtypes for ids and offsets,
  enums for state machines, typestate for phase-ordered resources (the
  buffer and token lifecycles), generation tokens where ABA lurks. A bug
  the type system catches costs nothing forever. A phase machine is an
  enum per phase, never a struct of `Option`s and `bool`s whose legal
  combinations live in a comment.
- **Explicitly sized integers** (`u32`, `u64`, `i64`) in every wire
  format, log record, index slot and counter. `usize` is for in-memory
  indexing, lengths and capacities; it never crosses a serialization
  boundary.
- **A format is specified, not inherited**: byte order, version, valid
  flags, length semantics, each field encoded explicitly. Rust struct
  layout and enum representation are not storage formats. Encoder and
  decoder limits agree, including headers, trailers, alignment and the
  largest representable record.
- **The smallest type that preserves the contract**: `Result` when an
  operation can fail, `Option` for legitimate absence, an enum for
  distinct outcomes. Never collapse corruption, refusal, absence and
  success into a boolean or a sentinel. `None` never means both
  "missing" and "wrong type"; a bounded or truncated read is its own
  variant.
- **A production `match` names every variant of its enum**: a wildcard
  arm silently accepts a new state or error.
  `clippy::wildcard_enum_match_arm` and
  `clippy::match_wildcard_for_single_variants` are denied at every
  `src/lib.rs`, `src/main.rs` and `src/bin/*.rs` root under `crates/`
  and `bins/` (ADR-0144 D1; `scripts/check-lint-scopes.sh`). The two
  reasoned exceptions: a foreign `#[non_exhaustive]` enum (`foreign:`,
  saying what the rest means) and the frozen `CommandId` list that
  ADR-0143's table deletes.
- **Never infer an outcome from encoded output** (a reply's first byte,
  a length, an empty buffer). The operation returns its effect as a type
  and the consumer matches on it.
- **Parse, don't validate, at trust boundaries.** A value from a client,
  a peer or a file is checked once, where it enters, into a type whose
  only constructor is that check (`BoundedKey`, `Deadline`, a decoded
  cursor). Inner layers take the type; they neither re-check nor assert.
  A receipt type carries ordering the same way: what may be served only
  after it is durable is constructed only by the durable write's return
  value.
- **Name the population.** Every counter, gauge and fold says whether it
  is per cell, node, namespace or pool, in its name or its type; no
  expression mixes two populations without an explicit fold.
- **No swappable arguments**: distinct newtypes or a named options
  struct for parameters that share a representation but have different
  meanings; an enum instead of positional booleans that select unrelated
  modes.
- **Narrow integers with `try_from` or a locally proven bound.** Choose
  checked, saturating or wrapping arithmetic deliberately: saturation
  may suit an approximate metric; it must not hide broken quota
  accounting. Identity and generation wrap needs a reuse proof or a
  typed refusal.
- **Define floating-point behavior** for NaN, infinities, signed zero,
  ordering and canonical encoding, and whether the contract needs exact
  bits, a total order or a tolerance; those are not interchangeable.

### Assertions

Assertions detect programmer errors; operating errors are handled, never
asserted. The only correct response to corrupt code is to crash:
assertions turn a correctness bug into a liveness bug and multiply what
DST and fuzzing find.

- **Assert arguments, return values, preconditions, postconditions and
  invariants.** Aim for two meaningful checks per data-plane function on
  average. Check relationships, legal transitions and impossible
  combinations, not just field ranges. A type-enforced guarantee needs
  no ceremonial assertion.
- **`debug_assert!` is the default; a release `assert!` is a deliberate
  promotion** for an invariant whose violation endangers durable state.
  A per-operation promotion carries a ≤ 1% A/B; a per-batch or
  per-checkpoint check records its scope and cost. Neither input
  validation nor an unsafe operation's safety precondition may rest on a
  check absent from release builds.
- **An assertion about a caller is a debt; an assertion about yourself
  is a proof.** A release `assert!` justified by "the caller checked" is
  how one command kills a node. When a function is touched, move each
  caller-claim to the boundary type that makes it true and delete the
  assert, or return a typed error. The best assertion is the one a type
  made unnecessary.
- **Pair checks across boundaries.** Assert what an internal producer
  emits; validate what a consumer receives. A decoder returns a typed
  error for bad external bytes even if our writer would assert before
  emitting them. Check enqueue/dequeue, encode/decode and
  reserve/release independently, so one mistaken assumption does not
  validate itself twice.
- **Check the states we forbid as well as the ones we allow**, and test
  transitions across that boundary: exact limits, stale identities,
  repeated completion. An assertion on the happy path alone is
  incomplete.
- **Split compound assertions**: `assert!(a); assert!(b);` fails more
  precisely than `assert!(a && b)`.
- **Assert compile-time relationships** with const assertions (record
  header sizes, ring slot alignment, enum discriminant ranges).
- **Every state machine keeps a written invariant inventory** in its
  interface documentation: what holds, where it is established, how it
  is enforced. A proof gap is an open finding; the inventory waives no
  missing check and is updated with the transition.
- Assertions are a net, not a substitute for understanding: build the
  model, encode it, explain it in comments, and let the simulator hunt
  what you missed.

### Panics and errors

- **Panics are for violated internal invariants and sanctioned fail-stop
  conditions only.** Input validation, protocol errors, admission
  pressure and recoverable I/O failures return typed errors.
  `scripts/check-panic-policy.sh` enforces the letter; you enforce the
  spirit. `unwrap()`/`expect()` on an operational `Result` is a review
  reject; an invariant-justified `expect()` is judged as an assertion.
- **Every release `assert!`/`expect()`/`panic!`/`unreachable!` in cell
  code is a row of `docs/release-assert-inventory.tsv`** (ADR-0107 D2;
  `scripts/check-release-asserts.sh`): `I` an invariant on the callee's
  own state; `C` a claim about a caller **with the enforcing check named
  as a symbol the gate resolves** (`path/from/root.rs:Type::method`; a
  renamed enforcing function is red; ADR-0107 first amendment); `F` a
  fail-stop the policy sanctions. The gate is red on a new, changed or
  vanished site and on a pointer that does not resolve, and every cell
  crate is audited (`U` means unaudited, never evidence of safety). A
  row does not turn an operating error into an invariant: flags,
  client-sized numbers and lifetime counters need validated bounds or a
  defined exhaustion or wrap policy before they reach an assertion.
- **A precondition on data that crossed a trust boundary is a check you
  owe.** `debug_assert!` is absent from `[profile.release]`, so a
  documented "callers must not pass X" leaves the shipping build
  unchecked and makes the same input a cell panic in debug. Enforce it
  where it can be enforced (normalize, bound, or return a typed error)
  and assert a **postcondition on what you emit**: a RESP line writer
  enforces framing even when a caller interpolates client bytes into an
  error message (ADR-0097).
- **All errors are handled.** An ordinary error path can cause a
  catastrophic failure, so error paths get tests, fault points
  (`scripts/check-fault-points.sh` checks that each declared point is
  fired and armed) and crash-matrix coverage. A DST arm or fault plant
  asserts its own **engagement**: a condition that never fired is
  `VACUOUS`, never green (ADR-0117), and engagement counters survive
  simulated restarts.
- **Every `Err` arm restores custody.** A lease, fd, pin, credit, ticket
  or flag taken before a fallible step is released by RAII on the error
  path, not by a paired call the next edit can skip. Each arm names the
  fault point that reaches it under interleaving; an arm no fault point
  arms is untested code on the path most likely to wedge a cell.
- **Classify the errno, never the syscall**: `EMFILE`, `EDQUOT`,
  `ENOSPC` and `EINVAL` have different owners and recoveries.
- **Preserve the meaning of failure**: keep the error category, add
  bounded context. Never turn corrupt storage into "not found", a failed
  write into success, or an ambiguous timeout into permission to repeat
  a non-idempotent operation.
- **fsync failure is fail-stop** (`scripts/check-fsync-fail-stop.sh`).
  Other unrecoverable failures follow their explicit fail-stop policy;
  no fallback silently weakens acknowledged durability.

### Publication and recovery

- **Name the publication point**: when a mutation becomes visible, what
  an acknowledgment promises, which failures may still occur. Validate
  and reserve first, then publish through the owning seam; an error
  means nothing changed, and a typed error after partial mutation is not
  atomic failure handling. For a multi-key or variadic write, validation
  is a pure pass over every argument and application is infallible: a
  loop that validates as it mutates is wrong before it is tested.
- **Durable before served.** Nothing is acknowledged ahead of the
  durability its contract promises. Catalog, topology and configuration
  that recovery depends on are persisted before any cell serves them. A
  move is snapshot → put → conditional delete, never take-then-put. What
  the data directory means (cell count, key-hash secret, format epoch)
  lives in the directory, not in a command-line flag.
- **Written, durable and applied progress are distinct**, each scoped to
  its owner and incarnation. A prefix advances only when every required
  predecessor is covered; a later completion or a larger sequence number
  proves nothing about earlier work.
- **Recovery and replay are first-class execution paths**: the same
  invariants as live execution, from the persisted contract, not new
  runtime defaults. Test replacement, deletion, namespace recreation and
  interruption at publication boundaries.
- **A checksum checks byte integrity**, not record identity, placement,
  ordering or semantic validity; validate those independently. Unknown
  formats and malformed committed data take the contract's refusal path:
  never guess, skip, or repair away acknowledged state.
- **Crash safety is an explicit write/sync/rename/directory-sync
  order.** Reclamation needs proof that every reader, snapshot and
  recovery path released the old version. Test cuts between steps; an
  orderly shutdown proves nothing.

### Unsafe Rust

Safe Rust is the default. If it can be expressed safely at equal
measured cost, the unsafe version is wrong. Target: under 2% of lines.

- **Posture** (ADR-0049, ADR-0121; `scripts/check-unsafe-roots.sh`):
  `#![forbid(unsafe_code)]` at every library and binary root of the
  workspace's packages (`src/lib.rs`, `src/main.rs`, `src/bin/*.rs`),
  except the crates that hold audited unsafe. Each of those stays
  `deny(unsafe_code)` at its root with `#[allow(unsafe_code)]` on
  exactly the audited `mod` items, or one whole-file inner allow, never
  on a function or block:
  - the audited leaves: `inf-simd`, `inf-alloc`, `inf-fabric`, and
    `inf-runtime`'s `affinity`, `cold`, `driver`, `executor`, `net`,
    `kqueue`, `uring` and `signal` modules;
  - the module-scoped regions `inf_doc::emit`, `inf_server::log_bytes`,
    `inf_probe::evict` and `inf_sim::{net, steel}`.
- **What the gate sees.** It refuses a root with no attribute, a `deny`
  root outside the audited leaf set, a listed leaf that went `forbid`,
  and any allow written exactly `allow(unsafe_code)` that is not
  module-scoped; in the crates those roots build, a new unsafe block
  outside an allowed item is a compile error in every profile. It does
  not read an allow in another spelling (a reasoned
  `#[allow(unsafe_code, reason = …)]`, `#[expect(unsafe_code)]`, a
  grouped allow), nor build scripts, fuzz targets, integration tests and
  benchmarks, which are crate roots of their own and some of which use
  `unsafe`.
- **Every unsafe block** has a concrete `// SAFETY:` argument, Miri or
  Loom coverage where applicable, and a reviewer who read the argument,
  not just the code. Under `crates/*/src` and `bins/*/src` it also has
  an entry in its crate's `SAFETY.md`;
  `scripts/check-safety-inventory.sh` refuses a file that uses `unsafe`
  and that the inventory does not name.
- **Every `unsafe fn`** documents caller obligations in `# Safety`:
  alignment, initialization, bounds, aliasing, provenance, lifetime,
  pinning and thread ownership where relevant. "The caller checked"
  names a check the safe API actually enforces.
- **Small unsafe regions, narrow safe interfaces.** A borrowed view
  never outlives its backing storage. Address stability comes from the
  allocation and lifetime contract, with `Pin` where needed; passing a
  reference promises nothing after the borrow ends.
- **Prove the kernel or another cell has finished with a buffer before
  reuse**: cancellation requested is not terminal completion. Generation
  tokens reject stale completions; they replace neither ownership nor
  memory-ordering proofs.
- **Miri checks the supported memory-safety paths, Loom the modeled
  synchronization.** Neither proves an unmodeled kernel, device or
  protocol contract: name those boundaries and the tests that exercise
  them.

### Tooling as safety

All compiler and clippy warnings are errors (`-D warnings`). The
mechanical checks are not bureaucracy; they are laws made cheap. The
`check` recipe of the `justfile` runs them: crate boundaries
(`scripts/check-dep-dag.sh`), the cell denylist, the fault-point and
fsync-fail-stop greps, the attribution-divergence gate, and the
`check-*.sh` gates named beside their rules in this document. A check
that silently checks nothing gives false confidence.

- **Never weaken a check to merge.** Change the law first (ADR) or fix
  the code.
- **A shell gate is the last resort.** When clippy, the type system or a
  generated table can carry a rule, move it there and delete the script.
  The clock ban is the model: clippy's `disallowed-methods` for
  `Instant`/`SystemTime` `now`/`elapsed`, libc and the TSC, proven on a
  planted-bypass probe (ADR-0106 D7; `scripts/check-clock-ban.sh`); an
  entry the lint cannot resolve is a violation, not a warning.
- **Every `check-*.sh` asserts its scope** (ADR-0106): a missing
  directory, an empty file set or a truncated scan is a failure, not a
  skip, and the success line discloses what was scanned.
- **An exemption is a per-site marker with a reason**, listed in the
  gate's output: `denylist-allow: <why>`, `panic-policy-allow: <why>`,
  and for the clippy-resolved bans `#[allow(clippy::disallowed_methods,
  reason = "<why>")]` on the statement or function, never on a crate or
  file inside cell code.
- **A gate that cannot go red is not a gate.**
  `scripts/check-scripts-selftest.sh` runs a planted violation through
  each gate inside `just check`.
- **CI evidence obeys the same rule.** A workflow never writes a number
  an instrument should have measured (the nightly's `sim_seconds=` lines
  come from the virtual clocks that advanced), and a sweep recipe's exit
  status is the verdict of every shard.
- **No test or DST feature on a normal dependency edge** (ADR-0107 D1;
  `scripts/check-shipping-features.sh`).
- **Document identities are checked** (ADR-0106 D15, D17;
  `scripts/check-doc-artifacts.sh`): one generated compatibility matrix,
  no tracked run output, no obsolete layout name or landed-ADR
  placeholder in this document or `docs/ARCHITECTURE.md`. The same gate
  requires every relative link in a published Markdown file to resolve
  to a published file inside this repository (ADR-0166 D5; the scope is
  on its OK line). The release job checks the matrix against its
  renderer before packaging it.
- **Published text is self-contained.** Every document, comment,
  documentation comment, printed message, script and configuration file
  states the rule itself instead of pointing into the project's
  unpublished records. An engineering reference, code and configuration
  may cite a bare decision identifier (`ADR-0087 D2`) beside the rule
  they state, never as a link and never in the rule's place. The copy a
  newcomer reads first carries no decision or story identifier. Nothing
  published carries a review, finding, lane or batch identifier or
  narrates a review: a review's result is stated as the rule it
  produced. The pointer and identifier rules are review obligations in
  this repository.

## Performance

Design for performance before implementation, then measure. A budget
guides design; it is not an achieved number. The goal is useful
throughput within latency, memory and durability contracts.

### First-principles budgets

- **Napkin math before code.** The 1000× win is found at design time.
  Sketch **network, disk, memory and CPU**, each in latency and
  bandwidth: bytes moved, dependent accesses, cycles, syscalls, fabric
  hops and durability barriers, per operation and per batch.
- **Weight cost by frequency.** Start with the largest contribution to
  the critical path or resource demand. Network and disk often dominate,
  but repeated memory misses or CPU work can cost more. Name the
  bottleneck and the observation that would disprove it.
- **Budget per operation and per byte.** A short-key GET, a large-value
  SET, a cold lookup and an expanded reply stress different resources.
  Include occupancy, concurrency, skew and background work: a hot key's
  owning cell can saturate while node-wide CPU looks idle.
- **Separate latency from throughput, and service time from queueing
  time.** Derive queue capacity and retained bytes from the admitted
  work and the wait bound. More in-flight requests create no device or
  CPU capacity.
- **State the scale envelope before choosing a structure.** Anything
  proportional to the dataset (a checkpoint, a rebuild, a whole-file
  rewrite, a boot audit, a stop-and-copy growth step) is named, bounded
  per slice, and kept off the foreground path. Construction, resize,
  invalidation, cancellation, teardown and recovery are in the analysis:
  an amortized O(1) operation can hold an O(N) pause, and the worst
  legal input belongs in the budget. Probabilistic identity is part of
  the envelope: write n²/2ᵇ next to the gate size.
- **The budget names the resource expected to saturate. A run where a
  different resource saturates is a finding before it is a number.**
  Model the queue before building it: depth = arrival rate × service
  time. A cap that never binds and a gate the device arithmetic cannot
  reach are design errors found on paper.
- **Know the device's unit of work** (barrier class, flush scope, queue
  depth, block size) before choosing the write path, and rehearse on the
  gate's medium: tmpfs fsyncs in zero time and proves nothing about a
  barrier.

### CPU predictability

- **Give the CPU coherent runs of work**: compact hot loops with regular
  access patterns. Hoist invariant decisions out of loops; select
  backend, CPU kernel and stable configuration at their established
  boundaries. Batch compatible work only where ordering and fairness
  permit.
- **Keep the common path direct.** Rare errors, diagnostics and slow
  paths sit apart from repeated work; no redundant parsing, hashing,
  bounds calculation or mode test. Branchless code, branch hints and
  forced inlining are hypotheses that need evidence.
- **Expose the data a loop needs**: slices, scalar parameters or narrow
  borrows, not a large mutable context. Keep repeated field loads and
  loop dependencies apparent. A method is fine when it gives the
  compiler and the reviewer the same clear information; removing `&self`
  is not itself an optimization.
- **Static dispatch in hot loops**; the registered command and engine
  seams at their boundaries. Repeated dynamic dispatch, a hidden clone
  or an incidental loop-local allocation needs a named acceptance
  criterion and an end-to-end A/B. Generics cost instruction cache:
  specialize only where the benefit pays for the code growth.
- **Shorten dependency chains.** Contiguous traversal and independent
  operations expose memory-level parallelism; pointer chasing and serial
  lookups prevent it. Prefetch only valid addresses under the ownership
  contract, and measure distance, wasted bandwidth and cache pollution.
- **SIMD** needs runtime feature detection, a supported fallback, and
  correct tails and alignment. Never read outside initialized,
  accessible storage to simplify a vector loop. When a win depends on
  vectorization, bounds-check elimination, copy elimination or register
  reuse, inspect the generated code on the supported target.

### Memory, caches, and locality

- **Memory is the product (L5).** Every allocation belongs to a named,
  counted domain, and bytes per key, document and entry are release
  gates. Measure payloads, indexes, metadata, allocator slack, buffers,
  connection state and retained work. Today CI's attribution-divergence
  gate fails when the attributed gauges miss the RSS growth of one
  document load by more than 10% (one in-process store, a fixed list of
  gauges); the gate campaigns' attribution rows compare node-wide sums
  with RSS at the same bar, each over its own list of gauges, and no CI
  workflow runs those. The target is that a node-wide `sum(domains)` vs
  RSS divergence above 10% fails CI.
- **Fit the working set to the work**: compact records, contiguous
  storage, hot fields apart from cold metadata. Compare array-of-structs
  and struct-of-arrays against the real access pattern. Alignment and
  padding are paid per entry; a cache-line-sized object is not
  automatically cache-efficient.
- **Keep allocation and mutation near the owning cell.** Account for
  NUMA placement, page faults, TLB pressure and memory bandwidth. Avoid
  false sharing at sanctioned cross-cell boundaries; padding and
  stronger atomics need a demonstrated need. Never introduce shared
  mutable data-plane state to save a local copy (L1).
- **Count every copy and every retained byte.** Zero-copy can pin a
  large buffer for a small reply; a bounded copy can release it sooner.
  Compare total bytes moved, peak live memory, lifetime complexity and
  tail latency before choosing. A cheap CPU copy can still be a
  memory-bandwidth bottleneck when multiplied by the workload.

### Batching, fairness, and tail latency

- **Batch every boundary (L3)**: syscalls, fabric hops, fsyncs,
  prefetches and reply flushes. `sqes/submit`, `cmds/iteration` and the
  grouping ratios are always-on tripwires; no claim cites a run with red
  tripwires.
- **Bound a batch by items, bytes, work and flush deadline.** Measure
  sparse traffic and saturation: waiting for a full batch never strands
  a lone request, and a full queue never starves timers, replies or
  durability progress.
- **Separate control plane from data plane.** Control decides; data
  flows. Validate configuration and build execution policy outside hot
  loops. Cold code may pay for a thorough check; it still has resource
  bounds and never blocks a cell.
- **Background work spends explicit CPU and device budgets**: expiry,
  eviction, checkpoints, compaction, reclamation. Budget the expensive
  inner work, not the outer item count, with resumable progress and
  fairness so small requests cannot starve maintenance.
- **Slow readers and saturated peers retain memory and delay progress.**
  Test them with bounded output, in-flight work and cancellation. An
  optimization that moves cost into an unmeasured queue has not removed
  it.

### Measurement and acceptance

- **Mechanical sympathy is measured (L4).** State the hypothesis,
  workload, baseline, target metric and regression limits before
  changing code. Make one attributable change; run the end-to-end A/B on
  the designated box. SIMD, prefetch, zero-copy, layout tricks and
  allocator changes follow the same rule. An unproven optimization stays
  behind a feature flag, and the flag never relaxes correctness.
- **Specify the instrument before it measures**: estimator and scope;
  resolution; a spread budget with a **same-binary control leg**; a
  liveness counter with a planted red. Suspect the instrument first: run
  the same binary against itself before blaming the product or crediting
  a fix. A gate needs a load-response curve, not a point sampled where
  the curve is flat. An error counted as a completed operation, a skip
  that exits 0 and a lifetime histogram are instruments that cannot go
  red.
- **Report throughput with p50/p99/p99.9 latency, memory, errors,
  refusals and batching**, over the regimes the change affects: size
  distribution, uniform and skewed access, local and remote routing, hot
  and cold data, durability, concurrency, pressure. Never improve a
  percentile by dropping work or excluding stalls from timing.
- **Apply utilization, saturation and errors (USE) to the server and the
  load generator.** Profiles and CPU counters explain deltas (cycles,
  instructions, branch misses, cache and TLB misses, stalled or off-CPU
  time); a better counter alone does not establish a better database.
  Never present a noisy or saturated generator's result as server
  capacity.
- **Record the run**: revision and dirty state, build features,
  toolchain, workload, affinity, SMT, governor/EPP, thermals, device
  configuration, and results with their spread. Raw output stays local
  and ignored. A public claim needs exact reproduction commands,
  baseline revisions, the clean-tree reference tier and 3–5 replicates
  under the evidence policy.
- **Record the verdict**: `Accepted`, `Rejected` or `Revised`, with
  evidence. A losing A/B is kept as evidence and **not merged**. A
  correctness fix may be `Correctness-only`; an unrun measurement is
  `Evidence-pending`, with the missing run named.
- **`inf-bench` proves in-house gates.** Disclose a workload the
  external generator cannot drive; never silently substitute
  instruments. A public number needs a recorded, reproducible run that
  meets these rules (L10).

## Developer Experience

Code makes the correct change easy to find, implement and verify.
Naming, cache invalidation and off-by-one errors get explicit rules
because they are recurring sources of database defects.

### Naming things

- **Rust conventions first** (`snake_case` functions, variables and
  modules; `SCREAMING_SNAKE_CASE` constants; `UpperCamelCase` types;
  acronyms as words: `Crc32Frame`, not `CRC32Frame`).
- **Get the nouns and verbs right.** A name that needs its own
  explanation is a draft. Prefer nouns that survive being spoken in a
  design review and written in a metric name (`replica.pipeline`, not
  `replica.preparing`).
- **Units and qualifiers go last, most significant first**:
  `latency_ms_max`, `budget_bytes_slice`, `expiry_fires_per_slice_cap`.
  Related names line up and sort together.
- **No abbreviations** in identifiers, loop counters and established
  domain terms excepted. The allowlist (ADR-0125 A5): `lsn`, `crc`,
  `ttl`, `ns` for namespace; `buf` and `cfg` (the standard library's and
  Cargo's spellings); `ctx`; `ckpt` (the checkpoint format's name);
  `ptr` inside the unsafe leaves; `idx` and `prev`/`next` as loop-local
  names. Anything else is renamed when its function is next touched;
  there is no naming gate. Long-form flags in scripts and CLIs:
  `--reference-box`, never `-r`.
- **Symmetric pairs**: `source`/`target`, `begin`/`end`. `src`, `dst`
  and `dest` are not used. Meaning outranks matching name lengths.
- **Allocator and handle names carry their contract**: an `arena` is not
  freed item by item; a `pool` item returns whence it came; a `lease` is
  returned before suspension.
- **One word, one meaning.** A cell is the execution owner; `shard`
  names only the on-disk per-cell directory family (`shard-N/`,
  `shard_dir`); a partition is a unit of data ownership. Submitted,
  written, durable and applied are distinct progress states. Use the
  owning contract's vocabulary in code, metrics and documentation.
- **Order**: module purpose and central types before implementation
  details, public API before its private helpers, grouped by
  responsibility so the file reads top-down. Visibility as narrow as
  possible.
- **Call sites show** borrowing, ownership transfer and fallibility.
  Options that affect durability, limits, ordering or format
  compatibility are passed explicitly: library defaults are not our
  contract.

### Comments and commits

- **Always say why.** A comment carries what the code cannot:
  constraints, rejected alternatives, the invariant protected, the
  reproducible benchmark and decision behind a trick. A comment that
  narrates the next line is noise; delete it.
- **Concise**: a complete sentence for a rationale or invariant, a short
  phrase for an obvious inline label. A nontrivial test explains its
  trigger, oracle and expected failure, not each assertion.
- **Cite the live rule, not its history.** A comment, documentation
  comment, printed message, and the comments of scripts and
  configuration state the rule they rely on and the invariant it
  protects. They may name the current decision bare beside it (`ADR-0087
  D2`), never in its place, and never narrate amendments, reviews,
  batches or finding numbers: that is what the ADR and Git are for. If
  the rule cannot be stated without its history, the ADR needs
  superseding.
- **One logical change per commit**, with a short one-line message
  stating the concrete change, so bisect and the sim A/B diff work at
  the granularity of a decision. Detailed reasoning, reproduction
  commands and results go in the pull request description. No model
  co-author trailers.

### Cache invalidation

- **One authoritative fact; explicit derived state.** Before adding a
  cache, decide whether recomputation is cheaper and simpler. A cache
  has an owner, a capacity bound, a lookup key, a validity rule, a
  reclamation path, a measured benefit and a stated memory cost.
- **Validity includes every dependency**: key and namespace identity,
  incarnation or generation, revision, schema, relevant options. Cached
  absence needs invalidation too. A TTL bounds staleness only where the
  API permits staleness; it does not establish coherence.
- **Invalidate at the owning mutation boundary**, and enumerate every
  path that changes the cached fact: replacement, deletion, expiry,
  eviction, rename, flush, namespace drop and recreation, rebuild,
  configuration change. One command's hook cannot cover a path that
  bypasses it.
- **Revalidate after suspension, callback or deferred completion.** A
  slot, pointer or integer id can name a different object after reuse:
  validate its generation and the fact being used. Copying a handle does
  not preserve the freshness of the observation behind it.
- **A hash, fingerprint or checksum is not identity.** Verify full keys
  and the required generation before overwrite, deletion, accounting or
  publication. A stale prefetch hint may cost speed; it never decides
  which bytes belong to a key.
- **Test hits and misses against the authoritative state through
  mutation and recovery**: negative entries, delete and recreate, stale
  replies, resume after invalidation.

### Off-by-one errors

Distinct names and types for distinct quantities:

| Quantity | Meaning | Boundary rule |
|----------|---------|---------------|
| `index` | Position of an existing element | `index < count` |
| `count` | Number of elements; may be zero | Last index exists only when `count > 0` |
| `offset_bytes` | Byte position in a buffer or file | Validate against the correct storage extent |
| `length_bytes` | Number of bytes in a span | Prove the whole span fits |
| `end_exclusive` | First position outside a range | `begin <= end_exclusive <= length` |

- **Half-open ranges `[begin, end)`**; an empty range is valid where the
  contract permits. For a nonempty prefix the last index is `count - 1`;
  `index + 1` counts elements through that index, not the collection.
  Keep `index`, `count` and `size` distinct in names and in casts, and
  check overflow when converting between them.
- **Validate a slice without overflow**: `offset <= buffer_len`, then
  `length <= buffer_len - offset`, or `checked_add` and reject failure
  before slicing. A successful narrow cast is not a bounds check.
- **Show division and rounding intent**: `div_ceil`, an explicit floor,
  or a comment proving exactness. Reject a zero divisor, check exact
  divisibility where required, and prove that alignment rounding and
  `count * element_size` are representable. Reuse checked helpers.
- **Define inclusive or exclusive** for deadlines, TTL expiry, cursor
  continuation, sequence numbers and watermarks. A storage offset names
  an entry's start, end or next position: the type and contract say
  which. Never compare across units or ordering scopes.
- **Exercise 0, 1, limit − 1, limit and limit + 1** where representable,
  and the integer maximum; empty and full buffers, exact page and frame
  boundaries, partial final chunks, wrap and reuse.

### Scope, aliasing, and time

- **Smallest possible scope**; compute a value **where it is used**.
  Every line between check and use is room for drift (POCPOU).
- **Narrow mutable access, one owner per authoritative fact (L1).** A
  local cached value needs the same freshness reasoning as a cache.
  Never hold a borrow alive to avoid a cheap recomputation.
- **Derive `Copy` only when implicit copying is cheap and semantically
  safe.** Review large by-value arguments and returns for stack use and
  copy cost. Cloning to satisfy the borrow checker can hide an ownership
  problem and a latency spike.
- **Zero every padding byte that crosses a trust boundary** (wire
  buffers, log frames). Initialize every transmitted byte and serialize
  only the valid length, never spare capacity or struct padding: a
  checksum faithfully protects stale bytes emitted by accident.
- **Time, randomness, disk, network and fabric effects are injected
  (L7).** Cell code holds no lock and never sleeps, and ambient
  `Instant::now()` or `rand::random()` there is a denylist violation
  (`scripts/check-cell-denylist.sh`: no locks, sleep or ambient time in
  cells), and for clocks a clippy error under any spelling
  (`UNIX_EPOCH.elapsed()` and `_rdtsc` included): it breaks the
  simulator's authority over the universe, the most valuable testing
  asset we own.
- **Hashing is deterministic too.** A new cell container is a capped
  type (above), and a cell-resident `HashMap` or `HashSet` not yet
  migrated never carries `std`'s per-process `RandomState`: it keeps
  `inf_foundation::BuildIntHasher` for internally generated keys
  (addresses, tokens, keyed-hash outputs), or is a `BTreeMap` when
  ordered traversal is required. User-controlled keys use the keyed-hash
  contract; a deterministic integer hasher is not a substitute. Hash-map
  iteration order never chooses state-changing execution order.
- **Distinguish injected monotonic time for waits from persisted
  wall-clock deadlines.** Define equality at expiry and behavior across
  a restart or a clock jump. Avoid tests that depend on a real sleep to
  establish order.

### Tests and review

- **Tests, fixtures, seeds and harnesses live in source.** Generated
  logs, profiles and reports stay ignored; record commands and results,
  not output archives. See [validation and reference
  hardware](validation.md).
- **Red first for a defect.** Keep the smallest failing reproducer, then
  test the fix at the layer that owns the invariant. If clients can
  reach the bug, keep a client or binary test too. Show that the
  intended assertion or oracle catches the pre-fix behavior.
- **Test contracts and observable outcomes, not a copy of the
  implementation**: an independent model, a byte-exact compatibility
  oracle, or a separately derived invariant. Include invalid inputs,
  refusal, partial I/O, cancellation, reordered completion and
  crash/restart where relevant.
- **An oracle is independent when it shares no code with what it checks
  beyond the wire format and `limits`. Every oracle has a canary**: a
  planted violation, run in the same lane, that must turn it red. An
  oracle that performs the server's work, compares a count where
  contents were promised, or cannot produce the fault it guards against
  encodes the bug. A model that does not dispatch on every command is
  blind to the one it skips. Run the oracle at the shipped topology: the
  multi-cell binary, not only the store tier.
- **A test's oracle reads its own process, and its clock bounds are hang
  bounds** (ADR-0200 D7). A verdict reads no count, free-space figure,
  pinned-memory budget or rate that another process moves, and a bound
  on wall-clock time ends a hang instead of deciding pass or fail. A
  test whose verdict must read one of those, or whose bound is its
  subject, runs alone and says why.
- **Generate the hostile inputs from the limits**: 0, 1, limit − 1,
  limit, limit + 1, the integer maximum, 2³² where a `u32` narrows,
  negative, non-UTF-8, repeated and contradictory options. A generator
  capped below the cliff proves the plateau.
- **Keep failing seeds, minimized inputs, exact commands, build features
  and expected outcomes.** A simulator pass proves only the modeled
  paths: distinguish model-only, runtime and real-reference evidence. A
  green test that never reached the relevant fault point closes nothing.
- **The right tool per claim**: unit and property tests for local
  contracts, fuzzing for decoders, Miri for unsafe memory, Loom for
  synchronization, DST and crash tests for histories. Verify the
  shipping feature set and the slim builds when affected; a test-only
  feature never alters the production contract.
- **Review failure and cleanup paths with the happy path.** A semantic
  refactor keeps an equivalence oracle that detects changed output,
  ordering or errors. State what ran, what did not, and why: a missing
  tool or a skipped campaign is not a passing result.

### Style by the numbers

- `cargo fmt` settles formatting: edition 2024, `max_width = 100`, Unix
  newlines, 4-space indentation. The toolchain is pinned
  (`rust-toolchain.toml`); MSRV moves by decision, not drift.
- Keep Rust's explicit braced control flow; never compress several state
  transitions onto one line.
- **One hundred columns is a hard limit**: it fits two files side by
  side, and nothing hides past a horizontal scrollbar. `cargo fmt
  --check` cannot see a long string literal, comment, attribute or macro
  body, so `scripts/check-line-width.sh` counts every line of the Rust
  files under `crates/`, `bins/` and `tests/` (ADR-0125).

## Dependencies

Dependencies are **few and deliberate**. Core data-plane crates stay
effectively free of third-party dependencies; an intentional edge
dependency has an ADR. Internal dependencies are allowed.

- `docs/dep-dag.toml` is the authority for allowed edges; a re-export
  never bypasses it. `scripts/check-dep-dag.sh` checks every dependency
  kind: active and reserved edges in both directions, a row for every
  package, dev exemptions printed (ADR-0025, ADR-0106 D16).
- `just deny` gates licenses and advisories: `cargo deny` on every lock
  that holds a third-party crate, the workspace's and each fuzz
  workspace's.
- Review a dependency's transitive graph, features, unsafe code,
  allocation, blocking behavior, determinism, portability, build cost
  and maintenance. Pin reproducible inputs and review updates as code
  changes. Disable unused features; optional engines stay absent from
  slim builds. A familiar API is no reason to import a runtime or an
  allocator into a cell.

## Tools

A small standard toolbox beats an array of special instruments: `cargo`,
`just` and the checked-in scripts are the interface. If a task needs a
new tool, prefer a small Rust binary under `bins/` over a shell script
that works on exactly one machine. Shell is for straightforward process
composition; complex state and parsing live in typed, tested code.

- **The documented command is the real entry point.** Run from the
  workspace root: `just check` and `just deny` for the required
  baseline, then the affected layer's validation. Local and CI
  invocations stay aligned.
- **Scripts** validate arguments, quote paths, use explicit working
  directories, preserve failures through pipelines, collect every
  child's exit status, and give subprocesses a bounded lifetime and
  cleanup.
- **Tests and tools use isolated temporary directories and explicit
  targets.** No fixed port, existing server or developer's data
  directory is assumed. Seeds, configuration and artifact paths are
  reproducible.
- **Diagnostics name the operation, the relevant bounded identifiers and
  the failure reason.** Never dump arbitrary values or unbounded buffers
  into logs: a failure gives enough context to reproduce it without
  becoming another source of resource exhaustion.

## The Last Stage

Keep revising until the design is simple enough to draw as a table, the
failure paths are explicit, the envelope is written down, and the
evidence matches the claim and can go red. When a rule needs to change,
give the reason and update its governing document; change a frozen
contract by ADR before implementation. Leave the next engineer a smaller
problem and a stronger proof.