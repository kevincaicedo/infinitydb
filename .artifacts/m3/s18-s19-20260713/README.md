# M3-S18/S19 development evidence — 2026-07-13

This directory closes the implementation/CI evidence owned by M3-S18
(document crash atomicity) and M3-S19 (document-domain attribution). It is
development-tier evidence from a disclosed dirty tree, not a reference-box
campaign and not a public performance claim.

## S18 verdict

The five plan-named cuts are reviewable data in `tests/crash-matrix/m3.toml`
and execute the production tag-6/tag-7 codec plus `open_cell_log` recovery:
delta append before fsync, the last delta before a covering full, a torn
full, checkpoint publication before old-segment truncation with a delta
tail, and every record-prefix class around a fuzzy overlap. The real
server-path test additionally pins one record for a two-match mutation.

The CI document scenario reuses the M2 server plane, SimDisk, fsync
watermark, checkpoint, and ack ledger. Its 24-seed CI slice and a 256-seed
development sweep were green with zero refusals and zero violations; the
same seed produces byte-identical traces. This closes S18, while S23/S24
still own the generalized replay-equivalence and 10,000-seed fleet gates.

## S19 verdict and risk

The four-site reporting chain is reconciled: per-store report fields,
field-wise namespace aggregation, INFO/tripwire publication, and the frozen
tripwire list. Parser/ingest/effect/freeze scratch and the path cache are
counted once at their owning store/cell. `JSON.DEBUG MEMORY key` reports the
honest per-key subset and is declared partial versus RedisJSON. Existing
benchmark attribution rows now print the document resident column.

The Linux CI workload loaded 65,536 documents through the real command,
parser, cache, and store path. Incremental disjoint domains were 70,322,614
B versus a 70,283,264 B VmRSS increase: 0.056% divergence (threshold <=
10%). The exact six-shape table is in `bytes-per-doc.txt`.

That table is deliberately not all-good news: the current tree form costs
2.626x idoc for the large row and 4.402x for the wide-array row. S19's job
is to make this impossible to hide, not to relabel it green. M3-S20 owns the
final corpus/placement measurements and S25 owns the reference-box memory
gate; both retain this as a named risk.
