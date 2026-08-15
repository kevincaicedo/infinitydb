# Document-leg summary (the `loadgen-doc_*.log` bulk streams are not committed)

The four document legs replay their `.resp` pipes continuously for 32 h,
so `redis-cli --pipe` emits one `All data transferred… replies: N errors: N`
summary per iteration — 1.38 M summary lines and **686 MB**, of which
`loadgen-doc_esec.log` alone is 568 MB. GitHub rejects files over 100 MB
and the content is fully reproducible from the committed
`ingest-manifest.txt` + the blessed corpus seed, so the same rule already
applied to raw `.resp` streams (commit `23610ff`) applies here: **the
derived evidence is committed, the bulk stream is not.** The logs remain
on the box at
`.artifacts/v0.4.0/soak-unified-20260814-0351/loadgen-doc_*.log`.

Extracted with:

```sh
grep -ac 'errors: ' "$f"                                        # iterations
grep -ao 'replies: [0-9]*' "$f" | awk -F': ' '{s+=$2} END{print s}'
grep -ao 'errors: [0-9]*'  "$f" | awk -F': ' '{s+=$2} END{print s}'
```

| leg | size | pipe iterations | replies | errors |
|---|---|---|---|---|
| `doc_ingest` (memory default) | 35 MB | 326,455 | 2,324,359,600 | **0** |
| `doc_read` (path reads) | 40 MB | 380,661 | 784,161,660 | **0** |
| `doc_mut` (scalar mutations) | 41 MB | 379,958 | 835,907,600 | 2,000 |
| `doc_esec` (durable everysec) | 568 MB | 292,148 | 2,080,385,908 | 14,071,111 |
| **total** | 686 MB | 1,379,222 | **6,024,814,768** | 14,073,111 |

**6.02 billion replies over 32.14 h ≈ 52,062 replies/s sustained** on the
document plane, concurrent with the KV and tiered planes on the same node.

## Error classification (exhaustive)

- **`doc_esec` — all 14,071,111 are `BUSY durable log staging is full,
  retry`**, the designed bounded admission refusal on a saturated
  durable-everysec namespace (0.68% of that leg's replies). No other
  error string appears in the file.
- **`doc_mut` — 2,000 `ERR could not perform this operation on a key that
  doesn't exist`**, confined to the first iteration: the mutation pipe
  raced the ingest pipe at startup before `gate-1KiB:*` existed. Exactly
  the loop's own count (2,000 `JSON.NUMINCRBY` frames), self-clearing on
  the next iteration.
- **`doc_ingest`, `doc_read` — zero errors across 3.1 billion replies.**

`docs_live` ran 13,875 → 14,240 throughout (`samples.csv`), so the
document plane was live for the entire measured window.
