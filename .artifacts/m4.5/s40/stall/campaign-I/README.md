# S40 campaign I — the 103 ms maximum: stall attribution at the memtier shape (in-house generator)

Rules written before the run (2026-08-22). Engine `e123e83`, reference
box, same data root class as the corrected S40 memtier row.

## Shape

`gate-run m4.5 --only-s40 --reference-box --cells 4 --pin-start 0
--replicates 3 --duration 60 --offered-ops 100000 --s40-keys 1000000
--leg-idle-s 40 --device-stat nvme0n1` — the memtier row's shape on the
in-house generator: 32 connections, pipeline 1, 1 KiB SET over a 1 M-key
space, `everysec`, 100 000 offered ops/s with latency measured from the
intended send instant (coordinated omission counted), 60 s per leg, a
fresh data dir + namespace per leg, 40 s idle before each. During every
leg the harness scrapes every cell's `INFO persistence` and
`/sys/block/nvme0n1/stat` every 250 ms; the generator records the send
instant of its maximum and the per-second maxima.

## What is decided

- **The S27 D5 wording at this shape** binds on `s40:max_ms_worst ≤ 50`
  (the worst of the three legs); `s40:offered_rate_achieved_x_min ≥ 0.9`
  is the validity bar (a saturated leg's max is a saturation number —
  excluded and said so).
- **Attribution** is a note per leg: the sample window (250 ms) the max's
  send instant fell in, every engine event in it (checkpoint in flight /
  published, rotation, manifest + truncation, zero-fill bytes, admission
  parks, frame waits, checkpoint offers deferred) and the device's
  write time and busy share over the window; one word by precedence
  (checkpoint → rotation → manifest/truncation → zero-fill →
  admission-park → device-busy ≥ 50 % → unattributed).
- The 103 ms memtier event is **reproduced** if any leg's max exceeds
  50 ms; the word then names the next story's subject. If no leg exceeds
  50 ms in three same-night legs, the memtier event stays a recorded
  one-of-three on that generator, with this row as the in-house
  control (the D5 bar is then met on the in-house generator at this
  shape, and not yet on memtier).
- `seconds_over_50ms_total` says whether a maximum is an isolated event
  (1) or a regime.

## Disclosures expected

No `fstrim`; ambient system redis (idle, 0.1 % CPU) present as in every
campaign of the day; drive state from the campaign log's header/footer.
