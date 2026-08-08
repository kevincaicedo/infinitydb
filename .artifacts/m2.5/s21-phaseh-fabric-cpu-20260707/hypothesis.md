# S21 Phase-H — fabric-CPU lever slice: hypothesis (stated before code, L4)

Date: 2026-07-07 · Owner: Phase H per ADR-0027 (addendum) / ADR-0029.
Baseline: **penalty 60.4% binding** (`m2.5/s21-binding-20260706/`, natural
2.461M vs all-local 6.212M, anchor 1.44× Dragonfly in-run) at commit
`1886263`. Cycle budget from `m2.5/s09-s21-cycle-split-20260706/`: local
2,122 cyc/op; **+4,050 cyc/remote op** (plane machinery ~839, allocator
~627, kernel ~615, codec+mesh ~319, hashing+misc ~356, tail ~950).

## Gate math

Penalty ≤ 40% ⟺ natural/all-local ≥ 0.60. With local at 2,122 and 75%
remote mix: mix budget = 2,122/0.60 = 3,537 cyc/op ⟹ remote op ≤ 4,008
⟹ **added cost must fall from +4,050 to ≤ +1,886 (−2,164 cyc/remote op)**
— if the local leg is unchanged. Levers that also speed the local leg
raise the bar proportionally (recompute at measurement; the anchor row
benefits either way and must not regress from 1.44×).

## Code-level findings the levers target (read 2026-07-07)

On the natural leg, `needs_fabric` defers every remote command to the pump,
and `pump_was_active` then defers *every* command on that connection — so
at 75% remote effectively the whole leg rides the deferred path. Per
deferred/remote op today:

1. `OwnedCmd::from_argv` — 1 malloc + full argv copy per deferred command
   (`plane.rs:1433`); freed after dispatch. Malloc/free pair per op.
2. `owned.slices()` — 1 `Vec<&[u8]>` malloc per dispatched command
   (`plane.rs:2147`).
3. `extract_keys_slices` — 1 `Vec` malloc per call, called **twice** per
   remote single-key op (`has_remote_key` at 2188, `first_key` at 2412);
   `SlotRouter::slot_of(key)` also computed twice (is_local + owner_of).
4. Fabric gate = `KeyedGate<u64>` over std `HashMap` with **SipHash**
   (`gate.rs:43`): 3 hashed map ops per remote op (register at
   `send_apply`, `complete` at FABRIC-IN, remove at waiter poll), plus
   `credit_waiters` (`WaitList<CellId>`, also SipHash) hashes on
   `wake_one(from)` for **every** reply even when no one waits (the map is
   almost always empty). Perf confirms: `hash_one` 1.32% +
   `DefaultHasher::write` 0.91% of the natural leg ≈ ~115 cyc/op-mix
   ≈ ~150+/remote op.
5. Reply pool bounds: `REPLY_POOL_MAX = 256` buffers vs a natural-leg
   working set of up to 64 conns × REMOTE_WINDOW 32 = 2,048 in-flight
   `Bytes` gate values — the pool exhausts and the overflow allocates
   (the "reply-Vec churn past the pool" bucket).
6. `dispatch_one` fetches conn state via two separate `with_conn` slab
   lookups per op (cx tuple at 2148, subscriber_restricted at 2163).

## Levers (cheapest decisive first) and their budgets

| # | lever | mechanism | addressable (cyc/remote op, hypothesis) |
|---|---|---|---|
| A | integer-key gate hashing | in-house folded-multiply `Hasher` for `KeyedGate`/`WaitList` (tokens are sequential u64s — no HashDoS surface; table is cell-local and trusted); empty-map fast path in `wake_one` | 120–300 (hashing bucket + probe latency) |
| B | allocation-free deferral/dispatch | pool `OwnedCmd` bufs on `Shared` (mirror `reply_pool`); inline-array argv in `dispatch_one` (heap fallback argc > 16); `extract_keys_slices` → iterator/inline buffer computed **once** per op (slot too); `REPLY_POOL_MAX` sized to the in-flight window (L5 note: 2,048 × ≤4 KiB caps = ≤8 MiB/cell worst case, actual sized by peak concurrency) | 350–650 (allocator bucket 627, minus glibc costs that survive in colder paths) |
| C | flattened dispatch | one `with_conn` fetch per op (cx + restricted flag together); single fabric borrow per send (token + stage in one scope; waiter registered before FABRIC-IN can run — publication happens at FABRIC-OUT, later in the same iteration) | 100–250 of the plane bucket (839) |
| — | tail sympathy | the 950-cyc tail is "the same plumbing" below 0.05% symbols — expected to shrink in proportion with B/C, not separately budgeted | unbudgeted |

**Combined hypothesis: −570 to −1,200 budgeted + tail sympathy.** Honest
statement: the budgeted mass may land **short of the −2,164 the gate
needs**. This slice is decisive either way: it empties the allocator and
hashing buckets, after which the residual is pump/kernel machinery that
only the deferred levers (batched window fill / one waiter registration
per batch; kernel wake path — needs `kptr_restrict=0` to split) can reach.
If the A/B lands short of ≤ 40%, the slice merges on throughput win +
penalty movement (correct direction, no anchor regression), and the gap
re-opens per ADR-0027 with the residual buckets named.

## Method

Implement levers as one tree (they are one lever *class*; per-bucket cycle
attribution separates their contributions), then:

1. Dev-tier perf sanity (cycles/op natural + local leg) before any binding
   spend — a losing tree never reaches the reference-box campaign.
2. Binding A/B, build-vs-build (baseline `1886263` from a scratchpad
   worktree vs lever tree), ABBA, ≥3 replicates/arm, `gate-run m0
   --reference-box`, anchor in-run, evidence committed between legs.
3. Post-run cycle split re-read (same `perf-campaign.sh`) to attribute the
   delta per bucket vs this table.

Semantics: all levers are behavior-neutral (allocation strategy, hashing,
borrow shape). No wire, ordering, or reply-byte change — compat suite
covers; loom not required (no `inf-fabric` concurrency-topology change);
the `KeyedGate` hasher change is `inf-runtime`-local and single-threaded
by construction (L1).
