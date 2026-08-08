# M2-S16 fault-point zero-cost A/B — dev tier, 2026-07-03

**Claim:** fault-point machinery is compiled out of release builds at
zero cost; the test tier (feature `fault-points`, unarmed) costs one
thread-local read + branch per site pass.

## Legs

1. `leg-a-off.txt` — `taskset -c 4 cargo bench -p inf-foundation --bench
   fault_fire` (default features = the shipping tier): fire cost
   **0.000 ns/call** — `fire()` is a `const false` the optimizer erases;
   the fire loop is byte-identical in time to the empty loop
   (0.295 vs 0.296 ns/iter over 2×10⁸ iters).
2. `leg-b-on-unarmed.txt` — same bench `--features fault-points`:
   **0.469 ns/call** unarmed (TLS read + is-empty branch). Context: the
   hottest production site is one `fire` per committed frame (≥ 32 KiB
   of log at steady state) — < 0.5 ns per 32 KiB is noise even in the
   test tier.
3. `binary-strip-proof.txt` — release `infinityd` (shipping feature set)
   grepped for the injected-fault string and all seven point names:
   zero data occurrences (the 6 `fsync_err` hits are debuginfo symbols
   of the pre-existing `on_fsync_error` production method — disposition
   in the file).

Env: Linux dev box (M0 profile), governor `performance`, pinned core 4.
Dev tier — supports the S16 AC; the S22 gate campaign re-runs all gates
on the default (compiled-out) build anyway.
