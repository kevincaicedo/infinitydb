# M2-S06 grouped `always` writes — driver-tier rehearsal (dev box, 2026-07-02)

**Status: dev-tier dry run — NON-CITABLE (L10).** The ≥ 300k w/s gate value
binds on the reference NVMe Gen4 box; this artifact rehearses the mechanism
and the measurement discipline (fsync histogram attached to every row), and
sizes what this dev box's device can group.

## What was measured

`inf-runtime/benches/log_fsync.rs` (`cargo bench -p inf-runtime --features
uring --bench log_fsync`): one `IoOp::LogWrite` (a group-sized synthetic
frame) + **linked fdatasync** per iteration against a 1 GiB `fallocate`d
file, sequential offsets, exactly one frame in flight (the staging-lease
discipline). Grouped-write throughput = group × fsync rate — the §8.2 group
commit model measured directly at the driver+device layer. The full-stack
row (staging → rotor → WatermarkGate under the reactor) binds at M2-S08/S22.

## Environment (disclosures)

- Dev box (see M0 Linux dev-box profile), kernel 7.0.0-27, io_uring modern
  mode, ext4 on consumer NVMe (`/dev/nvme0n1p3` — no power-loss protection,
  so fdatasync pays a full media flush ≈ 1.0–1.4 ms).
- **Governor: `powersave`** (interactive sudo unavailable in the session;
  `setup-infinity-benchmark-env.sh` pins `performance` for reruns). The rows
  are fsync-bound — device flush latency dominates, CPU frequency is
  second-order here.
- Load pinned to core 2 (`taskset -c 2`), 5 s/row, 3 replicates, tree dirty
  (mid-implementation session — dev tier only).

## Results (replicate 1; spread across 3 replicates < 1% on every row)

```text
 group   frames/s   fsyncs/s     writes/s     p50us     p99us    p999us     maxus
     1       1047       1047         1047       927      1727      2943      7110
    64        785        785        50255      1247      2015      2687      2728
   256        824        824       211012      1247      1983      2943      7602
  1024        760        760       778294      1375      2175      3007      3376
```

(replicates 2–3 in `replicates.txt`)

## Reading

- The group-commit lever works as modeled: fsync rate is flat (~760–830/s
  once frames carry real payload), so writes/s scales ~linearly with group
  size — **778k w/s at group 1024**, 211k at 256, on a consumer device.
- fsync-per-op (group 1) caps at ~1.05k w/s — the anti-pattern floor the
  `acks/s ÷ fsyncs/s` tripwire (S21) exists to catch.
- On this device the 300k gate value needs group ≈ 370+. A reference-class
  NVMe (PLP, fdatasync ≈ 20–100 µs) reaches it at far smaller groups; the
  gate row remains **Evidence-pending (reference box)**.
- Latency is honestly storage-bound: p50 0.93–1.38 ms, p99.9 ≤ 3.1 ms —
  the histogram every durable-mode claim must carry (§8.2).
