# M4.5-S36 — device budget — reference-box campaign (ADR-0088 D9) — 2026-08-21

**Tier: reference box (binding).** Same box and discipline as
`.artifacts/m4.5/s35-gate/README.md` (governor `performance`, cells pinned
0,2,4,6, generator 8,10,12,14, data on the ext4 NVMe, clean tree,
`env-check` OK, 40 s idle before every device leg, **no `fstrim`** —
sudo unavailable). `gate-run m4.5 --only-s36` (the S36 row: the leg A
KV 100 % write `everysec` shape at 32 conns × 1 KiB, 4 cells, 20 s
closed-loop with the server's CPU from `/proc/<pid>/stat`, the write-
amplification scrape at its end, a 10 s **offered-rate** leg at 100 000
ops/s — pipeline 16, latency from the intended send — and a 10 s tmpfs
control at the same shape). Every arm at the S35 default-flip candidate
`--barrier-class fua --frames-in-flight 3 --staging-mib 2`.

Probe file: `io-properties.reference-device.schema2.toml` — `inf
probe-device --seconds 3` on the campaign root at 15:44 (fua class,
`write_bytes_per_s_256k 510 MB/s`, `write_ops_per_s_4k 2 540`,
`write_ops_per_s_4k_qd4 3 597` — a blocking four-thread loop; the engine
sustained ~4× that per cell at K = 3).

| dir | what |
|---|---|
| `campaign-1/` | `b1f4bf4` — pre-zeroing still on for every `Direct` cell: budget-on vs budget-off (`--model-absent`). The two seal-pace arms died at spawn (`--seal-pace probe` on a server without a probe file) — fixed in `7750bfa`. |
| `campaign-2/` | `7750bfa` — pre-zeroing gated on an `always` namespace (ADR-0088 D5 amended): budget on/off **interleaved twice** (on-1, off-1, on-2, off-2) to separate drive state from the arm, then the seal-pace arm of the S36 row and the S35 row (`--only-s35 --seal-pace probe`). |

## Campaign 1 (`b1f4bf4`) — what the first row found

| arm | closed-loop ops/s | CPU % of 400 | max | write stall p99 | ckpts / 20 s | write-amp (milli) | offered 100k: achieved · p99 · max |
|---|---|---|---|---|---|---|---|
| budget on | 162 k | 172 | 454 ms | 37.9 ms | 14 | 1 471 | 99 998 · 188 ms · **301 ms** |
| budget off | 269 k | 222 | **1 871 ms** | 3.1 ms | 20 | 1 340 | 96 902 · 319 ms · **534 ms** |

Device bytes in 20 s, budget off: log 6.6 GB + **zero-fill 6.9 GB** +
checkpoint 1.05 GB ≈ 730 MB/s — the drive's sequential ceiling, with the
S34 pre-zeroing writing more than the log itself on a cell that has no
`always` namespace (pre-zeroing exists for write-through eligibility).
The budget's first row also found the checkpoint starving under
saturation (340 k deferrals, no publish in 20 s of 270 k ops/s) — the
**checkpoint keep-up floor** (ADR-0088 D2 amended) is that finding's fix,
and campaign 1 already runs with it (14–20 publishes per 20 s).

## Campaign 2 (`7750bfa`) — the binding set

| arm | closed-loop ops/s | CPU % of 400 | max | ckpts / 20 s | ckpt ÷ log (milli) | padding % of log | write-amp (milli) | offered 100k: achieved · p99 · max |
|---|---|---|---|---|---|---|---|---|
| budget on (1) | 200 k | 228 | 419 ms | 20 | 144 | 39 (derived) | 1 874 | 99 998 · 135 µs · **6.3 ms** |
| budget off (1) | 258 k | 267 | 158 ms | 29 | 169 | 38 (derived) | 1 891 | 99 997 · 139 µs · **4.2 ms** |
| budget on (2) | 298 k | **307** | 113 ms | 34 | 166 | 42 (derived) | 2 002 | 99 997 · 127 µs · **3.5 ms** |
| budget off (2) | 191 k | 204 | 408 ms | 19 | 146 | 32 (derived) | 1 689 | 99 997 · 135 µs · **3.8 ms** |
| seal pace (probe rate) | 276 k | 261 | 185 ms | 29 | 182 | 25 (derived; larger frames) | 1 586 | 99 997 · 131 µs · **5.1 ms** |

tmpfs control (flush class, every arm): 462–468 k ops/s at 402–403 %.
`zero_fill_bytes = 0` in every device arm (the gate); `parked` 3.3–6.9 k
per closed-loop leg, 0 at the offered rate.

### Readings

1. **S27 D5 (`max ≤ 50 ms`) at the comparator-matched offered rate: met
   in every arm — 3.5–6.3 ms** (p99 127–139 µs) at 100 000 ops/s, from
   301–534 ms in campaign 1 and 1.87 s at closed-loop baseline. The lever
   was removing the zero-fill's second write from an `everysec`-only
   cell; the budget's keep-up floor keeps the checkpoints completing
   underneath (19–34 per 20 s). The closed-loop max stays 113–419 ms in
   every arm: the device at its byte ceiling, drive-state dominated.
2. **CPU ≥ 300 % and ≥ 0.85 × tmpfs: not met.** Interleaved arms read on
   200 k / 298 k vs off 258 k / 191 k — the repeat-to-repeat variance
   (drive state without `fstrim`) exceeds any budget effect; the engine
   is device-bound at closed-loop saturation in every arm (204–307 %,
   0.41–0.64 × tmpfs), with log bytes at 340–540 MB/s of which **32–42 %**
   is v3 frame padding + framing (2.2 records × ~1.05 KiB per 4 KiB frame
   at K = 3 × 32 conns). **Correction (2026-08-21, review):** this README
   first said "~70 %"; that number was never in the reports (the row's
   `log_padding_pct` field landed in `417c821`, after these runs) and it
   does not reconcile with the combined figure (70 % padding would put the
   log term alone at ≥ 3.3×). The column above is *derived* per arm as
   `1 − append_bytes / log_frame_bytes` with `append_bytes = (log_frame_
   bytes + ckpt_bytes_total) / write_amp_milli`; the next S36 row emits
   `log_padding_bytes` and `log_padding_pct` raw.
   ADR-0088 D9's falsifier fired as written: the binding variable is the
   device's own write floor at this offered rate, not background I/O —
   the row is a device row; the comparator claim is "device-bound where
   they are device-bound" and needs the same-night comparators (not run).
3. **The checkpoint term is what S36 owns and it holds: 144–182 milli
   of log bytes (design bound 500, 1/α)** — against the 2× the diagnosis
   derived for the pre-S36 trigger. The combined `write_amp_milli_log_
   checkpoint` (1.59–2.00) is dominated by the padding term S34 disclosed
   (`log_padding_pct`), which is shape-dependent (frame size): the gate
   is decomposed accordingly (`s36_checkpoint_over_log_milli`, binding;
   the combined figure informational) — an L10 amendment with this
   evidence, not a narrowing.
4. **Seal pacer (ADR-0088 D2b) at the probed QD-4 rate: a losing A/B.**
   S36 row: no throughput or tail gain within the variance. S35 row
   (`s35-seal-pace-report.md`): 32-conn p50 1.0 ms vs 0.735 unpaced
   (1.63 × barrier — the gate fails), 31 k vs 39 k ops/s, 3.6 vs 2.2
   records per frame, @256 110–190 k with the same bimodal tail. The
   probe's blocking four-thread rate (899 barriers/s per cell) is far
   below what the engine's async submission sustains (~4 k/s per cell).
   `Rejected` at that rate; the knob stays (`--seal-pace N`), off.

## Limitations, stated

- No `fstrim`; drive state varies ±50 % between repeats of the same
  arm at this byte rate (≈ 10–15 GB written per arm). Interleaving
  bounds the confound, it does not remove it.
- The `write_stall_p99_us` of campaign 1's budget-on arm (37.9 ms) ran
  first on a fresh drive state; not reproduced.
- The offered rate (100 000 ops/s) is the S27 comparator median shape;
  the same-night comparator rows were not run.
- `tmpfs` control runs the flush class (FUA is a memcpy there); its
  number is the engine's CPU ceiling at the shape, not a durable figure.
