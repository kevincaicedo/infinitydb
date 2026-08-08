# M2-S10 slice-budget rehearsal — dev tier (non-binding)

date: 2026-07-02T21:44:20Z
kernel: 7.0.0-27-generic
cpu: i7-13700KF · governor: performance · EPP: performance
git: 78d4055 dirty=yes (S09/S10 working tree — the code under test IS the dirty tree; disclosed)
command: cargo test -p inf-server --release --test node_e2e -- --ignored ckpt_slice_budget_rehearsal --nocapture

## Result

- dataset: 240,000 keys x 1 KiB in one durable (everysec) namespace, one cell
- checkpoint: 238 MiB, 883 sections (256 KiB target), 240,000 post-image records
- wall duration under continuous foreground GET load: **0.13 s**
- foreground during the checkpoint: 6,720 GETs, **max 160 µs, zero > 2 ms**
  (client-observed round-trip; the anti-BGREWRITEAOF bar is p99.9 < 2 ms)
- `.ick` audit: loader reproduces the writer summary (sections/records/digest)
- ckpt_buffer_bytes returns to 0 at completion (bounded domain, L5)

## Caveats (why this is a rehearsal, not the gate)

- Foreground load = GETs from one connection, not the S12 saturating mixed
  write pressure row; the binding p99.9 < 2 ms gate measures there (S12/S22,
  reference box).
- 238 MiB resident set, not the 10 GB AC row — mechanism-tier evidence that
  budgeted slices keep the tail flat; the 10 GB histogram binds at S12/S22.
- `loop_iter_p999_us` scraped 0 in this harness (gauge not populated on
  this path) — client-observed latency is the reported signal.
- Section writes land in page cache (buffered + one completion fdatasync,
  ADR-0014's shipped tier); a device-saturated row may behave differently —
  exactly what S12 exists to measure.
