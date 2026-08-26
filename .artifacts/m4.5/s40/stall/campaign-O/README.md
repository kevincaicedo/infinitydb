# S40 campaign O — the stall-attribution row on the corrected generator (I2's row, rerun)

Written 2026-08-25 **before** the first leg. Reference box; tier per the
run's own `env-check` header. The review of campaigns I/I2 (2026-08-25)
found the offered-rate generator caught up after a stall (a burst above
the offered rate, latencies stamped from slots long past) and the
attribution read one 250 ms window around the maximum's *intended*
instant. Both are fixed in the harness of this session: a slot due on a
full pipeline is skipped and counted (`offered` / `sent` /
`skipped_pipeline_full`), the maximum carries its intended, actual and
completion instants, and the attribution lists every candidate class
present over the send-to-completion interval.

## Row — I2's, unchanged

`gate-run m4.5 --only-s40 --reference-box --cells 4 --pin-start 0
--replicates 3 --duration 60 --offered-ops 100000 --s40-keys 1000000
--leg-idle-s 40 --device-stat nvme0n1 --data-root ~/bench-data/s40/data-O`
with the reference device model at the data root. Harness on 8,10,12,14.

## What this run decides (predeclared)

- It re-reads the S40 stall-attribution clause the review marked
  `Evidence-pending` on the corrected instrument: per leg, the maximum,
  its interval, the candidate list, the skipped share.
- The S27 D5 `max ≤ 50 ms` bar is read as before (worst leg); the
  expectation on the record is that the drive's stall class recurs in
  some legs (I: 1 of 3; I2: 1 of 3) and the bar stays red for the
  device's reason — a pass would be drive state, not a fix, and is
  read that way.
- Validity: achieved ≥ 0.9 of offered on every leg (skipped slots now
  lower the achieved rate — a leg whose skipped share exceeds 10 % is a
  saturation reading by this rule).
- The p99.9 and the sub-50 ms maxima may read lower than I2's: the old
  generator's catch-up burst inflated them (stamped from slots long
  past); the difference is the instrument's, disclosed, not an engine
  change (the engine is `ada9a40`'s cell code).
