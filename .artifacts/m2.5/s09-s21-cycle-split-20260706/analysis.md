# M2.5-S09 / S21 — cycle-accounting campaign (2026-07-06)

One campaign, two artifacts: the S09 §18.1 stage decomposition (local leg)
and the S21 per-remote-op CPU split (natural − local delta) that the
remote-first A/B's discriminator answer demanded.

## Method

- box: designated reference box (i7-13700KF), governor performance, turbo
  off (3.4 GHz P-core base), `perf_event_paranoid=-1`, `kptr_restrict=1`
  (kernel samples counted but **unsymbolized** — disclosed below).
- server: release `infinityd --cells 4 --pin-start 4` (cells on cpus
  4/6/8/10); loadgen `inf-bench load` on cpus 12,14,16–23 (E-cores +
  2 P-cores, disjoint from cells), conns 64 × P=16, 1 M keys **filled
  first** (`--fill 1000000`), 40 s run. Loadgen **not** saturated: the same
  generator shape drives 6.41 M ops/s on the local leg.
- measurement: `perf stat`/`perf record -g` on the 4 cell CPUs for 10 s/8 s
  windows inside steady state (`perf-campaign.sh`, this dir); ops from the
  loadgen's own 40 s account. Cells are always-busy (spin fills any idle),
  but both legs are server-bound at gate-row rates (natural 2.64 M ≈ the
  2.46–2.51 M binding row; local 6.41 M ≈ the 6.21–6.59 M binding rows), so
  gross cycles/op is honest.
- tripwires in-run (INFO): natural `cmds_per_iter` 23.6, `fabric_msgs_per_batch`
  39.4, loop p999 85 µs, fabric RTT p50 135 µs; local `cmds_per_iter` 185.0.
  The natural loop turns ~8× faster with ~8× less foreground work per turn —
  per-iteration fixed costs amortize 8× worse under fabric cadence.

## Headline (cross-checked)

|  | cycles/op (node CPU / op) | ns @3.4 GHz |
|---|---|---|
| all-local | **2122** | 624 |
| natural mix (75 % remote) | 5159 | 1517 |
| **implied remote op** | **6172 (2.91× local)** | 1815 |
| **added per remote op** | **+4050** | **+1191** |

The 2.91× perf-derived multiple independently reproduces the 2.78×
throughput-derived multiple (hop-cost-decomposition.md) — two instruments,
one number. IPC: local 1.50, natural 1.95 (fabric code is branchy but
cache-friendlier than the store walk). LLC misses/op: local 10.2,
natural 12.6.

## S21 — where the +4050 cyc/remote-op goes (natural − local, per bucket)

| bucket (symbols) | cyc per remote op | note |
|---|---|---|
| fabric plane machinery (`pump`/`dispatch_one`/`send_apply`/`handle_fabric_op`/`render_outcome`) | **839** | per-op async plumbing, window bookkeeping, reply rendering |
| allocator traffic (`malloc`/`memmove`/`realloc`/`free`) | **627** | `OwnedCmd::from_argv` per deferred cmd, reply-Vec churn past the pool, gate value boxes |
| kernel (unsymbolized — doorbell eventfd writes/reads, sched, uring enters) | **615** | needs `kptr_restrict=0` (sudo) to split; bounded here |
| hashing + misc (incl. `std` `DefaultHasher` HashMap on the remote path) | 356 | a SipHash HashMap lookup per remote op — locate and replace |
| fabric codec + mesh (`encode`/`decode_frame`/`publish`/`drain`) | 319 | |
| executor/wakers | 118 · driver user-side 79 · parse +88 · reactor +51 · exec/ser +39 · plane other +109 | |
| unreported tail (symbols < 0.05 %) | 950 | long tail of the same plumbing |
| store | −138 | remote ops skip origin-side store work |

Reading: **no single hot spot — the added cost is distributed per-op
machinery**, exactly what the remote-first A/B's rejection predicted
(overlap loss is not the term; publishing earlier only splits packs).
Copies were already bounded < 2 % (S21 audit) and codec is small; the
levers with mass are (1) allocation-free deferral/reply path, (2) flattened
pump dispatch (batch the window fill, one waiter registration per batch,
not per op), (3) the kernel wake path, (4) the stray std-HashMap. Combined
addressable mass ≈ 2400–2900 cyc of the 4050 — a credible route from 60 %
toward the ≤ 40 % staged gate, but it is Phase-H implementation work, not
a flag flip.

## S09 — local leg vs the §18.1 budget (the gate math)

Measured 2122 cyc/op (node CPU per op, all-local, binding clocks) vs the
§18.1 budget total 300–500:

| §18.1 stage | budget | measured (bucket) | verdict |
|---|---|---|---|
| Parse | 40–80 | 164 (`inf_wire`) | 2–4× over |
| Dispatch | 10–20 | (inside plane/exec buckets, small) | ~ok |
| Hash + Index probe + Record access | 75–110 effective | **848** (`inf_store`, `resolve_hashed` 40 % self) | **~10× over — the dominant gap**; 10.2 LLC misses/op: the probe/record chain is NOT prefetch-hidden (the ADR-0005 pipeline is demoted/off) |
| Execute + serialize | 40–80 | 18 (exec+RespWriter) | under budget |
| I/O + loop amortized | 100–200 | kernel 388 + reactor 82 + driver 29 + libc 53 | 2–3× over (syscall side) |
| (unbudgeted) | — | other + tail 540 | bookkeeping the budget never named |

Verdicts the numbers force:
1. **The budget's miss-hiding assumption is violated** — "1 miss, hidden by
   batch prefetch pipeline" describes code that exists (`get_many`,
   ADR-0005) but is off the hot path; every op pays the walk scalar.
2. **The node clears the 6 M gate all-local today**: 6.21–6.59 M binding
   (three same-day gate-runs) — at 4 cells of the 8 the gate spec names
   (`m0-gates.toml` says 8 cells).
3. **The natural-routing row is the cross-cell penalty wearing the gate's
   name**: 2.46–2.51 M binding = all-local × (1 − 0.604). The pipelined
   gate and the S21 target are one debt, not two.
