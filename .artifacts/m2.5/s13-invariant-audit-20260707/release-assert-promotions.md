# M2.5-S13 — Release-assert promotion set

Concrete promotions for the invariants whose violation endangers durable
state (INFINITY_STYLE §Assertions: *"promotion to a release `assert!` is a
deliberate act for invariants whose violation endangers durable state, and
carries a ≤ 1% A/B (the M2.5-S13 rule)"*).

**Hot-path classification key** (which need the ≤ 1% A/B):
- **per-op** — runs once per command/record → **needs the ≤ 1% A/B**.
- **per-batch** — once per reactor LOG step (covers many ops) → effectively
  free; no A/B required.
- **per-slice** — once per MAINTAIN slice → free.
- **per-boot / per-checkpoint** — once per boot or per checkpoint → free.

**Headline finding: none of the five promotions is per-op.** The
durable-honesty invariants all live at batch/checkpoint boundaries, so
hardening them to release costs nothing measurable — the ≤ 1% A/B is not
triggered by this set. The one per-op release assert on the durable path
(`DurableCell::stage`'s `assert!(!self.failed)`, durable.rs:227) already
exists and is a single bool test.

Each proposed assert matches the codebase's existing idiom: message string,
and compound conditions split into separate statements
(INFINITY_STYLE §Assertions "Split compound assertions").

---

## P1 (headline) — `CkptCell::publish`: the `.ick` is durable before it is named

**File/function:** `crates/inf-server/src/ckpt.rs`, `impl<F> CkptCell<F>::publish`, line 220.
**Class:** per-checkpoint (**free**).
**Danger:** renaming `ckpt-N.ick.new → ckpt-N.ick` before its completion
fdatasync landed (or with a section write still in flight) publishes a
checkpoint whose tail sections are not fsync-durable. A crash after the
subsequent MANIFEST swap loads a short/torn recovery unit — silent durable
corruption.

Current:
```rust
debug_assert!(st.sync_done && st.in_flight.is_none());
```
Proposed (split per §Assertions):
```rust
assert!(st.sync_done, "publish before the completion fdatasync landed");
assert!(st.in_flight.is_none(), "publish with a section write still in flight");
```

---

## P2 (headline) — `GroupCommit::push_pending`: ledger coverage is monotone

**File/function:** `crates/inf-log/src/commit.rs`, `GroupCommit::push_pending`, line 498.
**Class:** per-batch (**free** — one push per registered fsync, ≤ a handful per LOG step).
**Danger:** a ledger entry whose `covers_up_to` is below the tail lets the
done-prefix advance the watermark past an earlier, less-covered entry →
`WatermarkGate` acks an LSN that was never fsync-covered. This is the single
invariant the entire watermark-honesty argument rests on.

Current:
```rust
debug_assert!(
    self.pending.back().is_none_or(|p| p.covers_up_to <= covers_up_to),
    "fsync coverage must be monotone in submission order"
);
```
Proposed — same condition/message, `debug_assert!` → `assert!`:
```rust
assert!(
    self.pending.back().is_none_or(|p| p.covers_up_to <= covers_up_to),
    "fsync coverage must be monotone in submission order"
);
```

---

## P3 — `GroupCommit::note_frame_queued`: frames queue in append order

**File/function:** `crates/inf-log/src/commit.rs`, `GroupCommit::note_frame_queued`, line 408.
**Class:** per-batch (**free** — one per sealed frame per LOG step).
**Danger:** a `queued_up_to` regression breaks the LSN↔durable-seq FIFO the
ack gate (`DurableCell::on_synced` → `frame_seqs`) and the reader assume;
acks would fire against the wrong frame.

Current:
```rust
debug_assert!(self.queued_up_to.is_none_or(|q| q < end), "frames queue in append order");
```
Proposed:
```rust
assert!(self.queued_up_to.is_none_or(|q| q < end), "frames queue in append order");
```

---

## P4 — `GroupCommit::register_linked_fsync`: no unqueued `always` record at discharge

**File/function:** `crates/inf-log/src/commit.rs`, `GroupCommit::register_linked_fsync`, line 432.
**Class:** per-batch (**free** — only on frames carrying `always` traffic).
**Danger:** clearing `always_pending` while an `always` record is still
unqueued means that record's ack gates on a linked sync whose coverage
(`queued_up_to`) does not include it → ack before durable (S06 oracle
violation).

Current:
```rust
debug_assert!(!self.always_unqueued, "linked sync with an unqueued always record");
```
Proposed:
```rust
assert!(!self.always_unqueued, "linked sync with an unqueued always record");
```

---

## P5 — `ManifestCell::note_published_ick`: one recovery-unit transition in flight

**File/function:** `crates/inf-server/src/ckpt.rs`, `ManifestCell::note_published_ick`, line 445.
**Class:** per-checkpoint (**free**).
**Danger:** starting a second swap while one is in flight could publish a
MANIFEST naming a unit whose begin marker is not yet watermark-covered
(the publication guard, 4.3, assumes a single serialized transition).

Current:
```rust
debug_assert!(self.idle(), "one recovery-unit transition in flight");
```
Proposed:
```rust
assert!(self.idle(), "one recovery-unit transition in flight");
```

---

## Optional additions (new `debug_assert!` for the inventory's gaps — not promotions)

These close UNCHECKED gaps found in the inventory. Keep them `debug_assert!`
(they guard bookkeeping/metrics, not durable bytes).

### A1 — `DurableCell::on_synced`: ack sequence is monotone (guards a real underflow)

**File:** `crates/inf-server/src/durable.rs`, `on_synced`, line 358.
**Class:** per-batch.
`self.group_hist_records.record(seq - self.acked_seq)` assumes
`seq >= self.acked_seq`. If the FIFO ordering ever regressed, this
subtraction **underflows** — a panic in debug (acceptable) but a wrapped
`u64` garbage histogram sample in release. Add before the subtraction:
```rust
debug_assert!(seq >= self.acked_seq, "ack seq regressed — frame_seqs FIFO broken");
```
(Once P2/P3 are release asserts the precondition is upstream-guaranteed, so
this stays a debug pair, not a release assert.)

### A2 — `CkptCell`/`ManifestCell` `DirSynced` commit: paired publication guard (optional)

**File:** `crates/inf-server/src/ckpt.rs`, `swap_slice` `SwapPhase::DirSynced` arm, ~line 643.
The `WatermarkWait` arm (ckpt.rs:553) already gates staging on
`watermark >= begin_lsn`. A *paired* assert at the commit point
(INFINITY_STYLE §Assertions "Pair assertions ... before writing to disk and
after") documents that the just-committed manifest's begin was covered. It
requires threading the observed watermark into the arm; **low value** given
the guard already prevents the bad path — list as optional, not required.

### A3 — `CellRecoverySlot::mark_ready`: called once

**File:** `crates/inf-server/src/control.rs`, `mark_ready`, line 60.
**Class:** per-boot.
```rust
debug_assert_eq!(self.state.load(Ordering::Relaxed), 0, "mark_ready called twice");
```

---

## Rollout note

All five promotions are pure `debug_assert!`→`assert!` edits (identical
condition + message), no new state, no borrow changes. Because every one is
per-batch/per-checkpoint, the M2.5-S13 ≤ 1% A/B gate is **not** triggered;
land them under the `Correctness-only` disposition with a one-line ledger
note ("durable-honesty invariants promoted to release; per-batch, no hot-op
cost"). The DST durable sweep (`just durable-sweep`) and the fsyncgate
crash-matrix child are the regression instruments — a promoted assert that
ever fires there is a caught corruption, exactly the intended outcome.
