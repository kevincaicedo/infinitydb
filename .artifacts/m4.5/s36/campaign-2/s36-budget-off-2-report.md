# M4.5 gate-run report

date: 1787345106 (unix) · binary target/release/infinityd · cells 4 · S36 row only · frames-in-flight 3 · barrier-class fua · staging-mib 2 · device-write-mbps probe-file · seal-pace off
env-check: OK
tier: reference-box (binding)

notes:
- data root: /home/kcaicedo/bench-data/s35-gate/data (ext4)
- durable arms: frames-in-flight 3 · barrier-class fua · staging-mib 2 · device-write-mbps probe-file · seal-pace off
- --only-s36: the S29, S27 and S35 rows were skipped; their gate keys are absent
- s36: the device model is ABSENT on the device arm (no schema-2 io-properties.toml              and no --device-write-mbps): background I/O unbudgeted — this is the pre-S36              baseline arm, not the budgeted one
- s36 row: flat everysec, 200000-key space × 1 KiB, 32 conns; device leg          20s closed-loop with server CPU from /proc/<pid>/stat (100 ticks/s); write-amp scraped          at its end; offered-rate leg 10s at 100000 ops/s (pipeline 16, latency from          the intended send); tmpfs control 10s on /tmp (tmpfs, flush class);          40s idle before every device leg

| gate | threshold | measured | verdict |
|---|---|---|---|
| S29: tiered always scaling slope (c256/c64) | >= 2 x (ops/s ratio across 4x conns) | — | PENDING (tooling) |
| S29: tiered:flat always parity at 64 conns | >= 0.7 x (tiered ops/s / flat ops/s, same node+session) | — | PENDING (tooling) |
| S29: tiered:flat always parity at 256 conns | >= 0.7 x (tiered ops/s / flat ops/s, same node+session) | — | PENDING (tooling) |
| S29: tiered:flat always p99 ratio at 256 conns | <= 4 x (tiered p99 / flat p99 — pre-fix read ~40x) | — | PENDING (tooling) |
| S27: client-visible -BUSY refusals under provoked staging pressure | <= 0.05 % of operations (ADR-0081 D5: pacing, not refusal) | — | PENDING (tooling) |
| S27: last:first throughput across back-to-back write repeats | >= 0.9 x (the finding's signature was 2.4x monotonic decay) | — | PENDING (tooling) |
| S27: worst per-leg max latency at everysec under provoked pressure | <= 50 ms (ADR-0081 D5: max <= 50 ms at everysec) | — | PENDING (tooling) |
| S35: always p50 over the barrier p50 at 32 conns | <= 1.3 x (client p50 / barrier p50; plan AC 1.2 + 0.1-window loop overhead — K = 1 reads ~1.85) | — | PENDING (tooling) |
| S35: N-cell vs 1-cell always p50 ratio (S34's F2 AC) | <= 1.3 x (N-cell p50 / 1-cell p50 at 32 conns; FLUSH read 1.8, FUA K = 1 1.45) | — | PENDING (tooling) |
| S36: server CPU across the pure-write everysec leg | >= 300 % of one core (400 = four cells flat out; pre-S36 read 123–185) | 204.48 | FAIL |
| S36: everysec pure-write throughput vs the same-session tmpfs control | >= 0.85 x (device arm ops/s / tmpfs control ops/s, same binary, same session) | 0.41 | FAIL |
| S36: log + checkpoint write amplification on the RAM-resident durable shape | <= 1600 milli-x ((log frames + checkpoint + MANIFEST bytes) / encoded record bytes; UNDEFINED until a checkpoint publishes) | 1689.00 | FAIL |
| S36: everysec max latency at the comparator-matched offered rate (S27 D5) | <= 50 ms (ADR-0081 D5 bar at an offered rate; latency from the intended send instant) | 3.75 | PASS |

## write amplification by row

Per namespace, worst first — never a node-wide blend (M4-S16).

| row | write amplification |
|---|---|
| device-budget | measured by this row as write_amp_milli_log_checkpoint (ADR-0088 D7: log frames +          checkpoint + MANIFEST bytes over encoded record bytes, cell scope, boot life; zero-fill          disclosed beside it) — the tier figure stays the M4 S16 row's |

## s36 per-leg samples

```
device closed-loop c32 everysec ops/s=191372   p99_us=175     max_us=408455            server_cpu_pct=204   parked=6080 write_stall_p99_us=1119
device arm: io_budget_model=absent ckpts_completed=19 write_amp_milli_log_checkpoint=1689          (undefined=0) log_frame_bytes=6846586880 ckpt_bytes_total=996368384 zero_fill_bytes=0          ckpt_interval_bytes(max)=268435456 deferrals[zero_fill=0 tier_flush=0 checkpoint=0]          frame_waits_pace=0
device offered-rate c32 P16 target=100000 everysec achieved ops/s=99997             p99_us=135     max_us=3753     server_cpu_pct=395   parked=0
tmpfs control (tmpfs) closed-loop c32 everysec ops/s=462068   p99_us=163              max_us=6004     server_cpu_pct=403  
```
