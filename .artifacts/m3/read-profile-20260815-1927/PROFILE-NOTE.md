# Why the raw `perf` sample file is not here

`jget-read.perf` — the raw `perf record` output this profile was extracted
from — was **1,080,502,860 bytes (1030.4 MB)**. It is excluded from git,
and `.gitignore` now excludes raw `perf` sample files across all artifact
directories so this cannot recur.

That is not a size dodge: GitHub hard-rejects any file over 100 MB, and
this one is 10× that. It blocked a push on 2026-08-16 and the commit had
to be rewritten out of history. The same rule the S24 phase-7 runbook
already states for campaign flamegraphs ("delete perf.data immediately")
now applies here structurally — `scripts/check-doc-read-profile.sh` drops
the raw file after extraction unless `KEEP_PERF_DATA=1` is set.

## What is committed, and why it is the evidence

| file | what it proves |
|---|---|
| `jget-read-report.txt` (7.0 MB) | the full `perf report` symbol table the gate greps — a reviewer can re-run the banned-symbol search against it without trusting our verdict |
| `banned-hits.txt` (0 bytes) | the grep result: **empty** — no parser or compiler symbol appeared on the read path |
| `verdict.txt` | `PASS: zero parser/compiler symbols in 2220 report rows` |
| `perf.log`, `load.txt`, `preload.txt`, `infinityd.log` | the run's own provenance |

The raw sample file adds no claim these do not already carry. It is
regenerable from nothing but the script and the box, and it is
machine-local by nature (call-graph samples keyed to one binary's load
addresses).

## Regenerating it

```bash
cd infinitydb
KEEP_PERF_DATA=1 scripts/check-doc-read-profile.sh <out-dir>
```

Precedent: the 20260814 soak bundle excluded 686 MB of repetitive
document-leg logs (one file 568 MB) the same way, behind a `.gitignore`
rule with `loadgen-doc-summary.md` carrying the exact counts.
