# INVALID FOR CLAIMS — placement contamination (disclosed per §19)

This run was launched under `taskset -c 12-23` on the harness with no
explicit cell pinning. Consequence: the load generators were confined to
the E-core set while the S19-closure baseline ran them unconstrained —
generator-bound rows read ~32–37% low (pipelined 1.92M vs 2.84M baseline,
unpipelined ratio 2.01 vs 3.21, cross-cell anchor 1.10 vs 1.72) while
placement-insensitive rows reproduce exactly (RSS 0.61x, attribution
2.8% vs 2.7%, penalty ratio 61.4% vs 64.3%). The run is labeled invalid
for regression comparison; the corrected re-run uses the S19-closure
invocation shape (no outer taskset). Kept committed as the record of why.
