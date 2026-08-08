# M2.5-S11 — small levers: CRC32C interleave + adaptive truncation drain (2026-07-07)

Box: reference (i7-13700KF, governor performance, turbo off), pinned cpu 4.

## CRC32C 3-way interleave — **Rejected without implementation**

The cheapest decisive experiment is the ceiling measurement + path
arithmetic, not the implementation (L4 — a lever that cannot show an
end-to-end win is rejected before it is built):

- Ceiling reproduced on this box (criterion, pinned):
  dispatched **22.5 GiB/s @ 64 B · 10.3 GiB/s @ 4 KiB · 8.5 GiB/s @
  64 KiB · 8.4 GiB/s @ 1 MiB** — the ≥ 64 KiB serial-`crc32q` regime
  ADR-0011 recorded (8.4–9.1 GiB/s), which the 3-way interleave+combine
  would roughly double.
- **Log frames never reach that regime**: group formation p50 ≈ 100
  records ≈ 6–12 KiB per frame checksum (S07 artifacts). At ~10 GiB/s a
  10 KiB frame costs ~1 µs against a 2.5–5 ms device fdatasync — CRC is
  ~0.02–0.04% of the durable path. Doubling it moves nothing measurable
  end-to-end at one frame/iteration; the plan predicted exactly this
  outcome ("the expected outcome is a recorded rejection, and that is
  fine — L4").
- The only ≥ 64 KiB consumers are `.ick` sections and recovery replay
  validation. Replay is **device-bound at 1.16 GB/s** (S08, gate met);
  CRC at 8.4 GiB/s is not the binding term. **Re-open condition, named:**
  if S18/Gen4-class hardware lifts replay past ~4 GB/s and CRC becomes
  the measured binding term, ADR-0011's interleave+combine remedy
  re-opens with this artifact as its baseline.

## Adaptive truncation drain (ADR-0022 D8.4) — **Accepted**

- Mechanism: `manifest_slice`'s truncation budget now follows the covered
  backlog — `max(MAX_UNLINKS_PER_SLICE, ⌈backlog/2⌉)` capped at
  `MAX_TRUNC_PER_SLICE_ADAPTIVE = 16` — instead of the fixed 2/slice.
  Each unit is rotor bookkeeping + one *delegated* unlink request (no
  device work on the loop, ADR-0017 discipline unchanged); the ceiling
  keeps it a slice, never a burst (L3).
- `truncation_bounds` e2e: **green in dev (2.3 s) and release (1.4 s)
  profiles** — previously the release profile filled the 16 GiB `/tmp`
  tmpfs (GiBs of unreclaimed log) and broke every shell on the box. The
  test is now byte-bounded as well (8192 trickle ticks ≈ 32 MiB, then
  poll-only until the latched checkpoint/manifest cycles land): its disk
  footprint is bounded by construction, independent of profile speed.
- Foreground-protection shape unchanged: the budget rides the same
  Maintenance deficit scheduler; no hot-path change, so no A/B row is
  owed beyond the e2e (the slice was and stays MAINTAIN-resident).
