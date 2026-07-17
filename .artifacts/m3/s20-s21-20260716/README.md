# M3-S20/S21 development evidence — 2026-07-16

This bundle closes the engineering-story gates for the reference document
corpus, document-root batch prefetch, and the pinned RedisJSON differential
oracle. It is development-tier evidence from a disclosed dirty tree, not a
reference-box or public performance claim.

## Disposition

- **S20 Accepted.** The checked manifest is the byte witness for all six
  seeded shapes. The 10,000,000-document ABBA campaign measured the full
  record+root pipeline at **9.567262 Mops/s** versus **3.420476 Mops/s** with
  prefetch off (**+179.706%**) and **6.295211 Mops/s** with record-only
  prefetch (**+52.269%** for the document-root pass). Both comparisons clear
  the story's +10% gate.
- The inline threshold remains **512 B**. The 256 B and 512 B candidates
  produce the same corpus placement and attributed bytes. Moving to 1,024 B
  saves 2.017% attributed bytes and improves random reads, but reduces the
  TTL rewrite row 27.105%; 2,048 B saves 3.530% and reduces that row 45.344%.
  No workload weighting authorizes that trade, so the smallest correct move
  is to retain the existing 512 B default.
- **S21 Accepted.** The exact Redis Stack image with ReJSON/20809 executed
  84 ordered cases under both RESP2 and RESP3: **168 executions, 148 exact,
  20 explicitly allowlisted, 0 failures**. The canary proves an unallowlisted
  mismatch fails, and the live run proves stale entries fail too.

## Evidence map

- `environment.txt` — checkout/profile/thermal disclosure.
- `corpus.txt` — generator, manifest, and shared-consumer checks.
- `prefetch.txt` — raw threshold and 10M-document ABBA output.
- `redisjson-oracle.txt` — exact pin, run summary, fixups, and deviations.
- `validation.txt` — focused and workspace validation.

S25 still owns the reference-box latency/RSS comparison against RedisJSON,
all M3 exit-gate numbers, and any public claim.
