# Operating tiered storage

> [!WARNING]
> Tiered storage lands in **`v0.4.0-alpha`**. The data plane is wired
> (M4-S26): string-family commands, `SCAN`, and `DBSIZE` serve tiered
> namespaces over TCP; the remaining gaps (blob writes, expiry) are
> marked where they occur rather than describing a system that does not
> exist. Nothing here
> is a performance claim — measured figures name their box and their tier,
> and the published numbers live in
> [`docs/claim-ledger.md`](../../docs/claim-ledger.md).

A **durable-tiered namespace** holds a dataset larger than the RAM you
gave it. Records live in one monotonically-growing logical address space
per namespace per cell: newest records in RAM, oldest on NVMe, and the
boundary moves as the namespace outgrows its memory budget. Reads of
RAM-resident records cost what they cost in a cache; reads below the cold
boundary suspend the command on an asynchronous disk read and resume it.

A **memory-mode namespace pays none of this.** It has no address space,
no region, no flush pipeline, and no counter object — the machinery is
absent, not disabled. Every `tiering_*` field in `INFO` reads exactly `0`
on a node with no tiered namespace, and that is asserted as a release
blocker, not hoped for.

- [The address space and its four watermarks](#the-address-space-and-its-four-watermarks)
- [Budgets and what they bound](#budgets-and-what-they-bound)
- [The write path, and why a byte is written twice](#the-write-path-and-why-a-byte-is-written-twice)
- [`INFO tiering` field reference](#info-tiering-field-reference)
- [What to alarm on](#what-to-alarm-on)
- [Tuning: the slice budget decides tier write amplification](#tuning-the-slice-budget-decides-tier-write-amplification)
- [Tuning: the compaction trigger sets the *other* half](#tuning-the-compaction-trigger-sets-the-other-half)
- [Not here yet](#not-here-yet)

## The address space and its four watermarks

Addresses only ever grow. Four watermarks cut the space into three
regions, and every record's life is a walk from right to left:

```text
0 ─────────────────────────────────────────────────────────────────▶ tail
┌──────────────────────┬─────────────────────────┬──────────────────────┐
│  COLD (NVMe)         │  READ-ONLY REGION (RAM) │  MUTABLE REGION (RAM)│
│  tier-NNNNNN.itier   │  immutable;             │  in-place updates    │
│                      │  update = copy-to-tail  │                      │
└──────────────────────┴─────────────────────────┴──────────────────────┘
     ▲ head        ▲ flushed        ▲ ro_boundary              ▲ tail
```

The order `head ≤ flushed ≤ ro_boundary ≤ tail` always holds. Read the
watermarks as answers to four questions:

| Watermark | Reads as | Moves when |
|---|---|---|
| `tail` | "where the next record will be written" | every write |
| `ro_boundary` | "everything below me is frozen — an update down there copies to the tail instead of rewriting in place" | a demotion **seal** slice |
| `flushed` | "everything below me is durable in a tier file, fdatasync'd" | a **flush** slice, after its barrier — never before |
| `head` | "everything below me is disk-only; its RAM pages are returned to the OS" | a **release** slice, never above `flushed` |

Two consequences worth internalizing, because they are what makes the
design safe rather than merely fast:

- **Nothing above `ro_boundary` ever flushes.** A record that can still be
  updated in place must not have a disk copy, or the disk copy would
  silently go stale.
- **No page is released above `flushed`.** Dropping RAM for bytes that are
  not yet durable is data loss, so the release slice cannot outrun the
  flush even under memory pressure — it backpressures instead.

The distance `tail − head` is the namespace's RAM footprint. The distance
`tail − ro_boundary` is its *mutable* footprint: the working set that can
still be updated without a copy.

**Addresses are per boot life.** Recovery re-appends the WAL tail at fresh
addresses, so watermark values are not comparable across a restart, and
neither are the counters below — all of them reset to zero at boot. Only
*content* is durable; addresses are bookkeeping.

## Budgets and what they bound

| Setting | Bounds | Default | Configurable |
|---|---|---|---|
| memory budget | committed RAM pages for the namespace (the `tail − head` window) | set at namespace creation | `INF.NS … MEM-BUDGET` (M4-S19, ADR-0062); hot-raisable only within the reserved ring — growth past it is drop + recreate |
| mutable fraction | how much of the budget stays updatable in place | 25% (250‰) | `MUTABLE-FRACTION` (permille, 1..=999); hot — applies to future sealing only |
| MAINTAIN slice | bytes one seal/flush/release step may move | 1 MiB (one commit page) | `MAINTAIN-SLICE` (64 KiB..=64 MiB); hot; see the tuning section — this one matters more than it looks |
| disk budget | tier files + blob extents on disk | 0 (unbounded) | `DISK-BUDGET` (0 or ≥ 1 MiB); hot; compaction pressure engages at 7/8 of it, foreground admission stops at 95% of it (`DISKFULL` — M4-S21, ADR-0063; the 5% gap is the compaction reserve) |
| compaction trigger | dead-ratio a file must reach to compact | 50% | `COMPACTION-DEAD-RATIO` (clamped 50..=100 — below 50 the write-amp gate is at risk by construction, the S16 canary measurement) |
| compaction slice | bytes one copy-forward round may move | 1 MiB | `COMPACTION-SLICE` (64 KiB..=64 MiB); hot |
| blob threshold | value size that routes out of line | 16 MiB | `BLOB-THRESHOLD` (4 KiB..=16 MiB); hot, future writes only |
| cold-read queue depth | per-cell cold-read cap | 64 | `COLD-READ-QD` (1..=4096); **create-only** |
| tier I/O mode | O_DIRECT vs page cache | `direct` (ADR-0054) | `TIER-IO-MODE` (direct\|buffered); **create-only** — per-file at open |
| stall timeout | max wait on the flushed watermark | 1000 ms | `TAIL-STALL-TIMEOUT` (1..=60000 ms); hot |
| tier file capacity | data bytes per `tier-NNNNNN.itier` | 1 GiB | still a construction parameter (the ADR-0056 file-size knob rides a later release) |
| node reserved-VA limit | aggregate tiered ring reservations across namespaces | 256 GiB | `CONFIG SET tiered-reserved-va-limit` (node-level, divided per cell; admission-only — ADR-0062 D4) |

A namespace is durable-**tiered** when its `INF.NS CREATE … MODE
durable` carries `MEM-BUDGET` — tiering is a configuration, not a mode
(ADR-0062 D1). `INF.NS SET <name> KEY value …` hot-reloads the keys
marked hot above; `INF.NS INFO <name>` reads the whole block back.
`INF.NS USE` selects it for the connection (M4-S26); the string family,
`SCAN`, and `DBSIZE` serve it. Three deliberate refusals, stated here so
nobody debugs a "missing" feature: expiry (`SET … EX`, the `EXPIRE`
family) refuses typed — tiered namespaces carry no TTL wheel in M4;
values at or above `BLOB-THRESHOLD` refuse until the blob write leg
lands; and every other command family answers
`ERR this command is not supported on tiered namespaces in M4`.
A write that outruns flush progress parks on the tail-stall gate and,
after `TAIL-STALL-TIMEOUT`, fails typed with `STALLED` — retryable,
bounded, never a hang. A cold `DEL` fetches and verifies its record
first (one NVMe read): the reply count is exact and the per-file
dead-byte accounting stays byte-exact — the TTL wheel and eviction, by
contrast, never issue cold reads (§3.3).

The memory budget is a bound on **resident** bytes, not live bytes: it
counts the committed ring window, which is what the operating system
charges you for. When writes outrun demotion, a write that would push the
window past its budget does not silently grow the process — it waits for
the flushed watermark to advance (`tiering_tail_alloc_stalls` counts
those waits) and retries. Backpressure, not an unbounded queue.

## The write path, and why a byte is written twice

**A user byte on a durable-tiered namespace is written twice, by design.**
Once as a WAL record — the log is the durability mechanism, unchanged
since `v0.2.0`, and it is what makes a crash survivable — and once as
tier-file bytes when the record's address range flushes to NVMe. Neither
copy is redundant: the WAL is how the write is *acknowledged*, the tier
file is how the record is *stored*. Later, compaction will add a third,
smaller term when it copies live records forward to reclaim dead space.

That is the baseline. It is not an apology and it is not an implementation
detail to be tuned away — a design that wrote once would be either
non-durable or a database that cannot reclaim space.

The four counters make it measurable rather than assumed:

| Counter | Counts | Does **not** count |
|---|---|---|
| `user_bytes` | key + value bytes of every record image the namespace admitted, measured at the record boundary | record headers, WAL framing, protocol bytes |
| `wal_bytes` | encoded WAL record bytes, length prefix included | the frame header/trailer shared by every namespace in that frame |
| `flush_bytes` | bytes the tier flush handed the device: frames, their CRCs, per-file header and footer blocks, rewrites of a partial tail frame, **and the re-flush of every record compaction relocated** | nothing the block layer sees for this namespace's tier files |
| `compaction_bytes` | bytes copy-forward compaction **relocated** — the volume it moved | the device write those bytes cause; that write is already in `flush_bytes` (see below) |

Write amplification is `(wal + flush) / user`, per namespace — and the
server reports it for you (`write_amp_milli`, next section).

**Why `compaction_bytes` is not in that sum.** Compaction does not write
to the disk. It copies a live record forward into memory at the tail, and
the ordinary flush then carries it to disk like any other record — so
`flush_bytes` already counted it. Adding `compaction_bytes` on top would
count every relocated byte twice, and the accounting would stop matching
what the device actually did (measured: +13% off, against −2% for the
formula above). Compaction's cost is fully inside the reported ratio; the
counter sits beside it to tell you **why** `flush_bytes` is high.

Four more things worth knowing before you read a ratio:

- **Deletes push it up, correctly.** A tombstone stores no user byte and
  costs a WAL record, so a delete-heavy workload reports amplification
  above the baseline. That is a true statement about the workload.
- **A namespace that only deletes reports `undefined`**, not `0`: it
  wrote bytes and admitted none, so the ratio has no denominator. Zero
  would read as "no amplification", which is the opposite of the truth.
- **It is per namespace, never per node.** A node-wide average hides one
  runaway tiered namespace behind a quiet one. The only aggregate the
  server publishes is a **maximum**, for exactly that reason.
- **It resets at every restart**, like every counter here.

Measured end to end against `/proc/diskstats` — the same counter `iostat`
reads — on 512 MiB of user bytes at 8× the memory budget:

| Workload | reported WA | counters vs the block layer |
|---|---|---|
| inserts only (no dead bytes, no compaction) | **1.999×** | −1.40% |
| skewed overwrites with compaction running | **1.730×** | −2.27% |

Artifacts:
[`.artifacts/m4/s13/accounting-vs-block-layer-20260725.md`](../.artifacts/m4/s13/accounting-vs-block-layer-20260725.md)
and [`.artifacts/m4/s16/README.md`](../.artifacts/m4/s16/README.md).
The churn row being *lower* is not a mistake: when writes are skewed,
many overwrites replace a record that is still in the mutable region, and
an in-place update costs no tier byte at all. Both are **dev-box**
measurements with the device deviation disclosed in the artifacts; the
gate-grade figure is the M4-S24 campaign's, and no public claim exists
until it has a ledger row.

## `INFO tiering` field reference

Two shapes. The `tiering_*` fields are cell aggregates; the
`tiering_ns<id>:` lines are per namespace and are what the aggregates are
the exact field-wise sum of.

Values below are from the insert-only validation leg (512 MiB of user
bytes into a namespace with a 64 MiB memory budget), line-wrapped here for
width:

```
# Tiering
tiering_tables:1
...
tiering_user_bytes:536903324
tiering_wal_bytes:543097988
tiering_flush_bytes:530038784
tiering_compaction_bytes:0
tiering_written_bytes:1073136772
tiering_write_amp_milli_max:1999
tiering_write_amp_undefined_ns:0
tiering_ns31:head=526385232,flushed=527433840,ro_boundary=527433840,
tail=545163516,committed_bytes=18874368,budget_bytes=67108864,
live_bytes=545162876,dead_bytes=640,user_bytes=536903324,
wal_bytes=543097988,flush_bytes=530038784,compaction_bytes=0,
write_amp_milli=1999
```

Read that namespace line as a sentence: 545 MB of addresses allocated,
of which everything below 526 MB is disk-only; 18 MB of RAM committed
against a 64 MB budget (demotion is comfortably ahead of the writes);
1 MB written but not yet released; 640 bytes dead; 1.999× write
amplification, which is the write-twice baseline. Note `head`, `flushed`,
`ro_boundary`, `tail` ascending — that order is the invariant the whole
design rests on.

The same namespace under skewed overwrites with compaction running (the
second validation leg) reads differently in the two places that matter:

```
live_bytes=68141040,dead_bytes=331703056,user_bytes=536870880,
wal_bytes=543065544,flush_bytes=385630208,compaction_bytes=146470896,
write_amp_milli=1730
```

The live set has collapsed to 68 MB while 332 MB of dead bytes await
reclaim, 146 MB of live records have been relocated by copy-forward — and
write amplification is *lower* than the insert case, because most
overwrites landed on records still in the mutable region.

**Aggregate fields**

| Field | Meaning |
|---|---|
| `tiering_tables` | durable-tiered namespaces on this cell (`0` ⇒ every other field is `0`) |
| `tiering_tail_allocs` | records allocated at the tail |
| `tiering_tail_alloc_stalls` | writes that waited for flush progress — the backpressure tripwire |
| `tiering_seal_holes` / `tiering_seal_hole_bytes` | ring-top seals and the dead bytes they skipped |
| `tiering_region_commit_pages` / `..._decommit_pages` | RAM pages taken from and returned to the OS |
| `tiering_cold_resolves` | lookups that landed below `head` — cold-read candidates |
| `tiering_cold_p50_us` / `..._p99_us` / `..._p999_us` | service-time percentiles (µs, loop clock) of tiered commands that issued at least one cold read — end-to-end command service, not device time |
| `tiering_ram_hit_split` | **disclosure, not a number**: reads `unmeasured-iteration-clock` while a tiered namespace is live. RAM-hit service is quantized to the reactor-iteration clock — a command that never suspends records 0 µs whatever its true service time — so the `tiering_ram_hit_p50/p99/p999_us` fields render **absent** rather than as silent zeros (they stay literal `0` on a memory-mode node, per the degenerate contract). A gate or dashboard that wants these numbers must wait for a finer injected clock, not read zeros |
| `cold_read_p99_us` | p99 enqueue→delivery latency of cold **device reads** (µs, injected loop clock; delivered reads only) |
| `coalesce_ratio_milli` | `1 − device_reads/logical_reads` × 1000 (ADR-0055 D5): `0` = no merging, higher = more device trips saved. Raw counters (`cold_reads_enqueued`, `cold_reads_issued`) stay exposed |
| `cold_pool_dry` | drain stalls with QD headroom but no free pool buffer — **pool-sizing pressure**, distinct from the policy cap; never an error |
| `cold_queue_full` | typed cold-read enqueue refusals on a full class FIFO (`BUSY` backpressure to the client) |
| `tiering_demote_slices` / `tiering_demote_sealed_bytes` | seal steps and the bytes they froze |
| `tiering_flush_slices` / `tiering_flush_confirmed_bytes` | flush barriers and the bytes they made durable |
| `tiering_reserved_bytes` | virtual address space reserved for the region rings (not RSS) |
| `tiering_committed_bytes` | **RAM actually committed** — the number the memory budget bounds |
| `tiering_allocated_bytes` / `tiering_dead_bytes` / `tiering_live_bytes` | address bytes allocated this life, dead bytes, live record bytes. **`dead_bytes` accumulates for the life and is never decremented** — it is "dead space *produced* since this life began", not "dead space sitting there now", so it keeps climbing while compaction reclaims normally. Do not read it as a reclaimable-space gauge, and do not divide it by `dead + live` and compare that to `COMPACTION-DEAD-RATIO`: the trigger is evaluated **per file** against that file's own current extent, so a node-level ratio drifts upward in any healthy long-running namespace (the v0.4.0 endurance run reached 80% node-wide while compaction ran 9.6 M slices). To ask "is reclaim keeping up?", watch `tiering_disk_used_bytes` **fall** and `tiering_compact_slices` **rise** |
| `tiering_index_bytes` | index + hash sidecar bytes |
| `tiering_user_bytes` · `tiering_wal_bytes` · `tiering_flush_bytes` · `tiering_compaction_bytes` | the four write counters, summed |
| `tiering_written_bytes` | `wal + flush` — the write-amplification numerator (the relocation volume is *not* added; see the previous section) |
| `tiering_write_amp_milli_max` | the **worst** namespace's write amplification × 1000 (`1730` is 1.730×). Never an average — an average would hide the namespace you need to see. `0` when no namespace has a denominator |
| `tiering_write_amp_undefined_ns` | namespaces that wrote bytes and admitted none (amplification unbounded). Non-zero means one of your namespaces only deletes — read its line before drawing conclusions from the maximum above |
| `tiering_diskfull_ns` | namespaces currently refusing writes for disk space (budget or device — M4-S21) |
| `tiering_diskfull_refusals` | typed `DISKFULL` refusals issued, summed |
| `tiering_compact_idle_pressure` | rounds where pressure asked compaction for space and **no file had a dead byte to reclaim** — the "genuinely full of live data" alarm |
| `tiering_disk_used_bytes` | `disk_used` at each namespace's last admission recompute, summed — the snapshots admission actually enforces, not a live `statvfs` |

**Per-namespace line** (`tiering_ns<id>:` followed by `key=value` pairs):
`head`, `flushed`, `ro_boundary`, `tail` (the four watermarks, as logical
addresses), `committed_bytes`, `budget_bytes`, `disk_budget_bytes` and
`mutable_permille` (the configured budgets — M4-S19), `live_bytes`,
`dead_bytes`, the four write counters, `write_amp_milli` — this
namespace's write amplification × 1000, or the literal token `undefined`
when it has admitted no user byte — and the blob leg (`blob_user_bytes`,
`blob_bytes`, `blob_write_amp_milli`, `blob_extents_live`,
`blob_disk_bytes` — M4-S17/S18/S19), and the disk-admission quartet
(`disk_used_bytes`, `disk_full=none|budget|device`, `diskfull_refusals`,
`compact_idle_pressure` — M4-S21). The aggregates above are the
maxima/sums of these; the raw counters are on the same line, so you can
always check the division yourself.

## What to alarm on

Ordered by how bad it is when you see it.

| Signal | What it means | What to do |
|---|---|---|
| `tiering_tail_alloc_stalls` rising | writes are blocking on flush progress: the namespace is producing bytes faster than the tier can absorb them | check device write bandwidth first; then raise the memory budget or the MAINTAIN slice. A slowly-rising count under a write burst is the design working; a monotonically climbing one under steady load is not |
| `flushed − head` growing without bound | release is not keeping up, or a checkpoint walk is pinning release | expect it to drain when the checkpoint completes; if it does not, the release slice is starved |
| `tail − flushed` growing without bound | the flush is behind the seal, so durability lag is growing and so is RAM | same causes as stalls, one stage earlier — this is the leading indicator |
| `committed_bytes` at `budget_bytes` continuously | the namespace is living at its ceiling; every write is one stall away | raise the budget or accept the backpressure — this is a capacity decision, not a fault |
| `tiering_write_amp_milli_max` above ~3000 (3×) | one namespace is amplifying past the release gate | find it on its `tiering_ns<id>:` line, then check in this order: the delete rate, the MAINTAIN slice (next section), and the compaction dead-ratio trigger — a trigger that fires at a low dead ratio copies many live bytes to reclaim few, and each copy is flushed again |
| `tiering_write_amp_undefined_ns` non-zero | a namespace wrote bytes and admitted none — amplification is unbounded there, and the maximum above does not describe it | expected for a delete-only namespace; unexpected otherwise, and then it is a bug report |
| `disk_full=budget` on a `tiering_ns<id>:` line | the namespace hit its `DISK-BUDGET`: new writes refuse `DISKFULL`; reads, deletes, expiry, and in-place updates continue | if `compact_idle_pressure` is **not** rising, compaction is reclaiming and admission will reopen on its own — watch, don't touch. Otherwise the namespace is full of *live* data: delete keys or raise `DISK-BUDGET` (hot) — those are the only two levers that exist |
| `disk_full=device` on a `tiering_ns<id>:` line | the device itself returned ENOSPC on a tier write | free space on the filesystem; the next MAINTAIN flush retry clears the latch automatically — there is no operator resume step, by design |
| `tiering_compact_idle_pressure` rising | disk pressure is asking compaction for space and there is not one dead byte to reclaim | this is the honest "capacity, not garbage" signal — no compaction tuning helps; delete data or raise the budget |
| `tiering_seal_hole_bytes` a material share of `tiering_allocated_bytes` | ring-top seals are wasting address space; expected to be well under 2% | report it — a large value means the ring is small relative to the record size |
| any `tiering_*` field non-zero on a cache-only node | a tiered namespace exists where none should, or the degenerate-case guarantee is broken | this one is a bug report, not a tuning knob |

Two counters that are **not** alarms: `tiering_cold_resolves` rising
simply means the dataset is bigger than RAM and cold reads are happening,
which is the feature; `tiering_reserved_bytes` is virtual address space,
not memory, and a large value is normal.

## Read-driven promotion (M4.5-S30, ADR-0085)

Residency is emergent from the log: written data congregates at the RAM
tail, untouched data ages to disk. Since M4.5-S30 **reads participate
too**: the second verified cold read of a key relocates the fetched
record back to the tail (a 64 KiB per-namespace second-touch filter
admits it; one-touch sweeps and `SCAN` never promote), so a bulk-loaded,
read-only working set warms instead of paying a cold read forever.
Promotion is unlogged (it reuses compaction's relocation machinery and
its replay repair) and strictly best-effort — it skips, never waits,
under a pinned checkpoint walk, a full tail window, the relocation-origin
cap, or disk-admission pressure.

- **Knob:** `CONFIG SET tiered-promote-on-read no|yes` (default `yes`,
  hot, node-wide). Turn it off for namespaces served by one-off readers
  where warming buys nothing — a per-namespace key is reserved, not yet
  wired.
- **Observability:** `tiering_promotions` / `tiering_promoted_bytes`
  (engagement; the per-namespace lines carry both), the
  `tiering_promote_skip_*` reasons, and `tiering_promote_first_touch`.
  A read-heavy namespace whose `promotions` counter sits at zero while
  `cold_reads_issued` climbs is either disabled or its reads are
  one-touch — both are answers, not faults.
- **Shadow-slot reconciliation (M4.5-S37, ADR-0093 — an arm, default
  off):** `CONFIG SET tiered-shadow-overwrite no|yes` (hot, node-wide).
  With it on, a plain `SET` whose only exact-hash candidate is cold no
  longer pays a foreground cold read: the record is appended and serves
  at once, the candidate stays slotted as a *shadow*, and a MAINTAIN
  reconciler reads and verifies it later (the same full-key comparison,
  the same exact death). The winner is pinned in RAM until then — the
  pinned suffix (`tiering_shadow_pinned_bytes`, capped at
  `MEM-BUDGET / 8`, `tiering_shadow_pin_cap_bytes`) and the open tickets
  (`tiering_shadow_pending`, ≤ 4 096 per cell-namespace) are the two
  bounds; at either bound the write verifies synchronously
  (`tiering_shadow_fallback_*`). `DBSIZE` counts keys, not tickets;
  `SCAN` never names a ticket's twin; `DEL`/`GETDEL` verify the twin
  first. Read `tiering_shadow_created` against `_resolved_same_key` /
  `_resolved_collision` (a real 64-bit collision keeps both records),
  `_stale` (the key moved past its read — re-offered), `_read_errors`
  (the ticket stays; the pinned suffix cannot release until it reads —
  visible, never fail-stop). Turning the knob off orphans nothing: open
  tickets keep reconciling. The default flips only on the reference-box
  campaign the ADR predeclares.
- **Cost model:** promoted bytes re-flush (they appear in `flush_bytes`,
  attributed by `tiering_promoted_bytes` the way `compaction_bytes`
  attributes copy-forward) and the displaced cold copy becomes dead
  bytes compaction reclaims. A converged working set stops promoting on
  its own — a permanently high promotion rate means the working set
  does not fit `MEM-BUDGET`.

## Tuning: the slice budget decides tier write amplification

A tier file's partial tail frame is rewritten in place at every flush
barrier until it fills up. So the MAINTAIN slice budget — how many bytes
one flush step moves before its `fdatasync` — decides how often a 4 KiB
frame is paid for twice. On the same workload:

| MAINTAIN slice | `flush_bytes` for 8.5 MB of user data |
|---|---|
| 4 KiB | 16.1 MB (≈1.9× the data) |
| 256 KiB | 9.5 MB (≈1.1× the data) |

(Measured by `flush_amplification_follows_the_slice_budget` in
`crates/inf-store/tests/tiered_accounting.rs` — run it yourself.)

The default is 1 MiB, which sits at the amortized end. The practical rule:
**if `tiering_flush_bytes` is running near 2× the data you wrote, you have
a slice budget to raise, not a bug to file.** The counters are honest
about the cost either way — that is what they are for.

## Tuning: the compaction trigger sets the *other* half

Compaction reclaims dead space by copying the live records out of a file
and deleting it. It fires when a file crosses a dead-ratio threshold — the
default is 50%. That threshold is the second knob write amplification
answers to, and the arithmetic is unforgiving: to reclaim one dead byte at
threshold `t`, compaction relocates `(1 − t)/t` live bytes, and every one
of them is written to disk again.

| Dead-ratio trigger | live bytes relocated per dead byte reclaimed | steady-state WA under sustained overwrites |
|---|---|---|
| 50% (default) | 1 | ≈ 3× |
| 25% | 3 | ≈ 5× |
| 10% | 9 | ≈ 11× |

Two consequences for reading a real number:

- **Lowering the threshold to "keep the disk tidier" is expensive.** It
  reclaims sooner and writes far more. Measured on a mis-tuned build at
  10%: 8.3× against 3.0× for the same workload at the default.
- **The default's ≈ 3× is a worst case, not a forecast.** It assumes every
  dead byte reached a tier file. Real workloads are skewed: hot records
  are usually overwritten while still in the mutable region, where an
  update costs no tier byte at all — the same measurement on a skewed
  workload reads 1.73×.

So if your write amplification is high, look at *what* is dying before
reaching over to a knob: a workload where every record cools before its
next update pays the full compaction bill, and no threshold makes that
free.

Both knobs are `INF.NS` keys since M4-S19: `MAINTAIN-SLICE` and
`COMPACTION-DEAD-RATIO` (the latter clamped to 50..=100 — the low end
that the canary measured at 8.3× is unrepresentable through
configuration, ADR-0062 D2).

## Disk-full behavior (M4-S21, ADR-0063)

At the cap the engine fails writes, never corrupts, and never silently
drops — the memory-pressure OOM discipline, applied to disk. Two error
shapes, both InfinityDB extensions (`DISKFULL` is the error class, the
`OOM`/`NOSPACE` convention):

```text
DISKFULL tiered namespace disk budget exhausted (used=<u> budget=<b>)
DISKFULL tier device out of space (ENOSPC)
```

What refuses and what does not: only operations that consume **new tier
bytes** refuse — fresh inserts, overwrites (copy-to-tail), and blob
writes. Reads (hot and cold), `DEL`, expiry, and in-place updates of
mutable-region records all proceed; deletes and expiry are how the
namespace frees itself. A `SET` on a hot key can therefore succeed at
the cap while the same `SET` on a cold key refuses — the budget bounds
bytes, and only the second asks for new ones.

The thresholds, in order of engagement: compaction pressure at 7/8 of
`DISK-BUDGET` (reclaim starts early), foreground admission stops at
95% — the remaining 5% is the **compaction reserve**, held open so
copy-forward can always write its way out of a full disk (flush and
compaction are never budget-refused; an admission check that gated them
would deadlock reclaim exactly when it matters).

**Recovery is automatic on every leg — watch, don't touch.** Budget
leg: reclaim, deletes, or a hot `DISK-BUDGET` raise reopen admission at
the next MAINTAIN round. Device leg: the flush retries its backlog
every round and the first successful barrier clears the latch. Crash
anywhere: unflushed bytes are WAL-covered, a torn flush resumes at the
confirmed watermark, an unreferenced extent is orphan-swept, and a node
recovering onto a still-full device boots, serves reads, and refuses
writes typed until space frees. A node that needed an operator to
resume admissions would be a bug report.

The four ENOSPC paths, for completeness: WAL append refuses typed
before the write (`NOSPACE`, the M2 discipline — unchanged); tier flush
backpressures then latches `DISKFULL (device)`; blob writes refuse
per-op (the failed extent's file is swept automatically); compaction
rides the reserve and, when there is nothing dead to reclaim, says so
(`compact_idle_pressure`) instead of rewriting live bytes for no gain.
Fsync-time failures are a different class entirely: tier/WAL fsync
failure is fail-stop (state unknowable — the fsyncgate rule), a blob
fsync failure abandons that extent typed.

## What the v0.4.0-alpha campaign measured (2026-08-15/16)

Numbers an operator can size against, re-read against this chapter after
the release campaign. Reference box, consumer **Gen3 DRAM-less NVMe**
(ADATA LEGEND 700, 476.9 GiB) — the device disposition travels with every
storage-bound figure here (ADR-0022 D4). Ledger rows in brackets.

| What | Measured | Gate | Read it as |
|---|---|---|---|
| Write amplification, worst namespace | **1.89×** on two tiered legs at different generator configs; **1.920×** independently over a 32 h endurance run | < 3× | Stable and load-independent — it is a counter ratio, so it does not move with offered load. If yours drifts past ~3×, work the `## What to alarm on` row for `tiering_write_amp_milli_max` [C33] |
| Memory over 24 h, three planes on one node | RSS slope **+0.234%/24 h**, accounted **−0.016%/24 h**, zero crashes, 32 h run | < 0.5%/24 h | Flat. Compaction ran 9.59 M slices and disk *fell* from a 38.73 GB peak to 30.30 GB [C31, C32] |
| 10 GB tiered node restart | **5.906 s** to first `PONG` | < 15 s | Comfortable, and this is the **worst** shape: no checkpoint had completed, so the boot replayed the whole tail with no `.ick` prefix to skip [C38a] |
| Per-cell replay rate | **0.266 GB/s/cell** (2.81 M records/cell in 5.906 s = **476 k records/s/cell**) | ≥ 1 GB/s/cell | **Gate not met.** Recovery is bound by *record count*, not device bandwidth — the same drive reads sequentially at 3.6 GiB/s. **Sizing consequence: your restart time scales with the number of records in the replay window, not its bytes.** A workload of many small records restarts more slowly than the same bytes in fewer, larger ones; checkpoint frequency is the lever you control [C38b, M4.5-S21] — since M4.5-S36 (ADR-0088 D4) the checkpoint interval is **derived** per cell (`clamp(2 × last checkpoint bytes, --ckpt-interval-bytes floor, replay rate × 5 s)` with a 2 M-record cap beside it, counted in on-disk frame bytes) and reported as `ckpt_interval_bytes`; since M4.5-S34's close-out (ADR-0088 D4 as amended, 2026-08-27) the replay rate is derived from the probe's **cold sequential read rows** — `min(read_bytes_per_s_256k_qd1, read_bytes_per_s_256k ÷ cells)` on a schema-4 `io-properties.toml` (one direct reader, and four direct readers aggregate), `read_bytes_per_s_256k ÷ max(cells, 4)` on a schema-3 file that lacks the one-reader row, and a 1 GiB/s constant only when the model carries no read row (ADR-0088 second amendment: conservative at every cell count; `ckpt_replay_bytes_per_s` / `ckpt_cap_bytes` in `INFO persistence`) — so on a four-cell node whose device reads ≈ 1 GB/s the byte cap is ≈ 1.2 GB of log between checkpoints, and a large dataset's checkpoint cadence (and its share of device writes) follows the device, not a constant; `inf cache-evict <data-dir>` evicts a data directory from the page cache (sync + `fadvise(DONTNEED)`, no root) to rehearse a power-loss-shaped restart; `--ckpt-interval-bytes` is the floor (0 = manual only). Since M4.5-S42 (ADR-0091) the probe runs **at the first boot of a data directory** (`--device-probe auto`, the default: ≈ 10 s once, a 256 MiB scratch file) and writes a schema-4 `io-properties.toml` bound to the device's identity (a moved directory is re-probed; `INFO persistence` reports `io_properties_source`); `inf probe-device <data-dir>` runs the same probe by hand, and `--device-probe off` is the dev tier (no file ⇒ `device model absent`, background I/O unbudgeted, the FLUSH class — never the production posture). The model enables the per-cell device budget (`io_budget_model:probed`) that bounds checkpoint, zero-fill, tier-flush and compaction I/O against the measured device |
| Cold-read p99 under load | **1.44 ms** at 8 conns / pipeline 1 · **3.65 ms** pipelined · 60–63 ms inside a saturated three-plane run | < 1.5 ms | **Gate not met under load**, disclosed. Cold-read latency on this device class is dominated by read/write interference — see the research note in `.artifacts/v0.4.0/cold-read-p99-research-20260808.md` [C35] |
| Hot set vs a RAM-resident node | p50 **12–22% faster**; p99 **+328%**, p99.9 **+40%** | within 10% | **Tail gate not met.** No tiering lookup cost exists — the profile shows no new hot-path symbol. Foreground commands occasionally queue behind maintenance and I/O completion on the same cell [C34a, C34b] |
| Cache p99 with three namespaces on one node | **+59% to +83%** vs the same namespace running alone | ≤ 10% | **Documented deviation.** The unified profile's real cost model. **Size for it**: if a latency-sensitive cache namespace shares a node with a document or tiered namespace, budget its p99 at roughly double its solo figure [C38d] |

### One counter that will not tell you what you expect

**`cold_read_qd_p99` does not rise under load on this hardware.** Probed
four ways during the campaign: this node at client pipeline 32 read
`qd_p99` **3**, at pipeline 128 still **3**; an `ycsb` connection sweep
from 8 to 64 connections moved it only **5 → 7** while throughput went
39.6k → 68.2k ops/s; and the 32 h endurance run read **5**. The ADR-0055
D2 admission cap is **64** and it never binds.

The reason is not that the queue is healthy — it is that queue depth here
is set by **device service time**, not by admission pressure: the cells
drain the cold queue about as fast as any generator can fill it. So
`cold_read_qd_p99` is a poor saturation signal on a device this fast
relative to the cell loop. Watch `tiering_cold_p99_us` and the foreground
latency instead; treat a `cold_read_qd_p99` anywhere near 64 as a genuine
alarm precisely because nothing in this campaign could produce one.

### The RAM-hit fields, and what the release gate actually used

`tiering_ram_hit_split` still reads `unmeasured-iteration-clock` and the
`tiering_ram_hit_p*_us` fields still render **absent** while a tiered
namespace is live — that part of the field reference above is unchanged
and correct. What changed is the *gate*: since ADR-0071 D2/D3 the
hot-set comparison is derived **client-side** from the cold-read counters,
and ADR-0071 D6 (2026-08-16) made its eligibility rules depend on which
leg is being measured. Two consequences for anyone reproducing the
measurement:

1. **Both legs must run at the same generator configuration**, and the
   comparison now refuses across differing ones. Client pipeline depth
   alone moved a derived memory-hit p50 by **12×** (22 µs at depth 1 vs
   279 µs at depth 8), because RESP replies are in-order and a cold read
   blocks everything queued behind it on that connection.
2. **Only skewed (zipfian) workloads can carry the row.** A uniform
   workload over 10× RAM reads 62–92% cold and is correctly refused — it
   has no hot set to serve at memory speed.

## Not here yet

Named so their absence is visible rather than mysterious:

- **Blob writes over the wire** — values at or above `BLOB-THRESHOLD`
  refuse typed until the blob write leg of M4-S26 lands (the extent
  machinery below is built and store-tier proven; the ledger-barrier
  wiring is the open half).
- **Expiry on tiered namespaces** — no TTL wheel in M4; the `EXPIRE`
  family refuses typed and `TTL` answers `-1` for live keys.
- **The per-file tier capacity knob** — still a construction parameter.

## Blob extents (M4-S17)

Values at or above the per-namespace threshold (16 MiB by default —
exactly the inline ceiling, so the default changes no existing value's
path; the knob is reserved for `INF.NS` as `BLOB-THRESHOLD`) store out
of line: one value per `blob-NNNNNN.iblob` file beside the tier files,
CRC-protected like everything else. The record in the log and in RAM
carries only a 24-byte reference — so copy-to-tail, flush, checkpoints,
and compaction move *references*, never the value bytes, and **blob
write amplification is ~1× by construction** (measured 1.001×: the value
reaches the device once, plus frame CRCs and one 4 KiB header).

Three facts worth knowing when operating this:

- **A blob write is durable before it is acknowledged.** The extent's
  bytes are fdatasync'd before the referencing log record can enter a
  commit batch. A crash between the two leaves an *orphan* — a durable
  file nothing references — which recovery's sweep unlinks in background
  slices. Orphaned disk after a crash is reclaimed automatically; it is
  never served.
- **Deleting or overwriting a blob key returns its disk after the
  delete itself is durable** — a short deferral (at most one group
  commit; up to the fsync interval in `everysec`), then the file unlinks
  in a MAINTAIN slice. `tiering_blob_extents_reclaimed` counts them;
  space is `statfs`-visible when it moves.
- **The blob fields in `INFO tiering`**: `tiering_blob_user_bytes` /
  `tiering_blob_bytes` are the blob leg's own denominator/numerator —
  deliberately **outside** the record write-amplification ratio (a byte
  is written once and counted once), and since M4-S18 the leg carries
  its own ratio: per-namespace `blob_write_amp_milli` (≈ `1001` by
  construction, or `undefined` for a namespace with no blob activity)
  and the cell aggregates `tiering_blob_write_amp_milli_max` /
  `tiering_blob_write_amp_undefined_ns` — the same worst-not-blend rule
  as the record leg, and the `inf-bench` report renders both legs side
  by side, never folded. `tiering_blob_extents_live` ×
  `tiering_blob_extent_bytes_live` is the out-of-line footprint;
  `tiering_blob_disk_bytes` is the device-byte view of it (live plus
  awaiting reclaim — what the disk budget counts, M4-S19);
  `tiering_blob_reclaimable` is the standing reclaim backlog and
  `tiering_blob_reclaim_deferred` counts non-fatal unlink deferrals —
  both drain to zero at quiescence, which is exactly what the S18 leak
  test asserts; `tiering_blob_rmw_ops` counts read-modify-write
  rewrites of blob values (zero until blob-resident documents wire up —
  extents are immutable, so a mutation rewrites the whole value; the
  counter exists so that cost can never hide).

---

See also: [Deployment](deployment.md) · [Architecture](architecture.md) ·
the milestone plan
[`docs/milestones/m4-tiered-storage.md`](../../docs/milestones/m4-tiered-storage.md)
for the engineering detail behind every mechanism named here.
