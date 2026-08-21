# M2 gate-run report

date: 1787332381 (unix) · cells: 4 · duration: 10s · ONLY-ALWAYS (A/B leg; frames-in-flight 1 · barrier-class fua · staging-mib 4)
env-check: OK
tier: reference-box (binding)

notes:
- pressure-data-root resolves under /tmp (tmpfs-likely): the everysec row writes ~13 GB with truncation disabled — a 16 GB tmpfs exhausts mid-row and the engine fail-stops per §8.4. Pass --pressure-data-root on a real filesystem.
- p99.9 deltas are quantized by LogHistogram (32 sub-buckets/octave ≈ 3%): 0.0% = same bucket; any non-zero delta spans ≥ 1 bucket
- M2 leg of the zero-cost rows runs without --data-dir (the memory-only assembly — the S09 posture: no durable plane constructed); the zero-record assert on a durable-enabled node is the node_e2e mixed-class test
- --baseline-bin not given: zero-cost delta rows report PENDING (build the pre-M2 commit's infinityd and pass its path)
- always row: 18753854 gated acks / 590163 fsyncs = ratio 31.8; fsync latency p50/p99/p999 = 113/335/415 us (HDR ~3% quantization); log_writes_per_iter 0.201 (590043 frames / 2937856 iterations); group formation p50/p99 = 51/123 records vs ~256 available in-flight writes/cell = 0.20x (M2.5-S07 gate: >= 0.8x)

| gate | threshold | measured | verdict |
|---|---|---|---|
| Zero-cost A/B: pipelined ops regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | — | PENDING (tooling) |
| Zero-cost A/B: pipelined p99.9 regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | — | PENDING (tooling) |
| Zero-cost A/B: unpipelined 512-conn ops regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | — | PENDING (tooling) |
| Zero-cost A/B: unpipelined p99.9 regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | — | PENDING (tooling) |
| Zero-cost A/B: ttl-heavy write-mix ops regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | — | PENDING (tooling) |
| Zero-cost A/B: ttl-heavy p99.9 regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | — | PENDING (tooling) |
| Memory-only rows append zero log records | <= 0 records | — | PENDING (tooling) |
| everysec penalty vs memory mode | < 10 % | — | PENDING (tooling) |
| always grouped writes | >= 300000 w/s | 1720819.04 | PASS |
| Replay throughput per cell | >= 1 GB/s/cell | — | PENDING (tooling) |
| 10 GB node cold boot | < 15 s | — | PENDING (tooling) |
| DST durability oracle: 10k seeds | <= 0 violations | — | PENDING (tooling) |
| Crash matrix green in CI | <= 0 failures | — | PENDING (tooling) |
| Checkpoint under full load: foreground p99.9 (anti-BGREWRITEAOF) | < 2000 us | — | PENDING (tooling) |
| RSS under continuous checkpoints vs no-checkpoint control (anti-2x) | <= 64 MiB peak-VmRSS delta (ckpt buffer domain is ~0.5 MiB/cell; a fork/COW would be dataset-sized) | — | PENDING (tooling) |
| M0/M1 gates re-pass | <= 5 % vs M1 artifact | — | PENDING (tooling) |
| One log write per iteration | <= 1 writes/iter | 0.20 | PASS |
| acks/fsync grouping ratio above floor | >= 2 acks per fsync | 31.78 | PASS (informational) |
| sum(domains) vs RSS divergence (with log domains) | <= 10 % | — | PENDING (tooling) |

## always grouped writes (always-grouped)

```
ops = 17210246
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 1720819
p50_us = 575
p99_us = 1183
p999_us = 1663
p9999_us = 2559
max_us = 3798
```
