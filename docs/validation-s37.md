# S37 reference validation protocol

Predeclared before measurement. This package resumes campaign Q and
ADR-0093 D9 and adds the L07 DBSIZE-under-tickets row. The runner is
`scripts/run-s37-reference.sh`; the measurement instrument is `inf-bench`.
These are in-house engine measurements; no competitor comparison is made.

## Measurement status

The previous reference attempt stopped at thermal preflight. Development
smoke did not meet D9's objectives; the default remains off. No acceptance
follows from moving this protocol out of generated output. Inspect the
current host and rerun; historical kernel and thermal readings are not a
live preflight. Results and limitations belong in the owning ledger.

## Frozen workload and validity

- Shipping binary, four cells on CPUs 0,2,4,6; generator on 8,10,12,14.
  These are disjoint physical P-cores; SMT remains enabled and is disclosed
  by `lscpu` and sibling lists in the environment artifact.
- Three replicates, alternating AB / BA / AB. Each arm uses a fresh data
  directory on the reference NVMe, one shared measured device profile, 1,000,000 keys of
  1 KiB, 128 MB per-cell tier budget, `FSYNC always`, direct tier I/O.
- Forty seconds of idle before durable legs **and after the fill**.
  Batch Q's five-second pre-fill idle did not settle its device before
  measurement. This correction must be named when comparing old evidence.
- Code and preparation documents must be committed before reference
  measurement. Logs and data first go outside the worktree, preserving
  clean-tree admission throughout the run. The runner records revision,
  binary hashes, version, topology, environment checks, five-second host
  samples, server stderr, raw INFO, and inherited 99 Hz DWARF profiles.
  Any failed command aborts the campaign. A successful process exit alone
  does not accept a result; the conditions below must also be reviewed.
  `inf probe-device` measures the profile once before the campaign, and
  that exact file is copied to every arm. Independent per-arm probes
  would change device budgets in addition to the intended shadow knob.

### Q — ticketed DEL memory and latency

`--s37-ticketed-del --s37-del-keys 12288 --s37-del-cycles 4`.
A disables shadow overwrite. B enables it and pauses reconciliation, so
each window's SET leaves tickets for its DEL pass. Both use 64 connections,
pipeline 1. Four disjoint windows give 49,152 DEL samples per leg. This row
measures DEL; it does not independently establish GETDEL response costs.

Validity: B's ticketed share at least 0.70, refusal zero, pending zero
after **every** pass, A's forced deletes zero. RSS growth difference B−A
and end-minus-base difference each at most 8 MiB. RSS is sampled every
20 ms, so sub-sample peaks are outside the instrument's resolution. Record
p50/p99/p99.9/max; tail has no acceptance threshold, and p99.9 above 50 ms
requires a stall disposition. Reconciliation stays paused only in Q and
DBSIZE, never in the D9 throughput campaign.

### D9 — shadow overwrite with reconciliation running

`--s37-shadow --s37-controls --read-leg-fill --duration 20`.
A disables shadow overwrite, B enables it. The c64/c256 write legs use
pipeline 1. Each arm also measures a filled, non-tiered `always` namespace
at c256 for its parity denominator, then the S35 200,000-key filled hot
GET shape at c64/P16 twice. GET errors and misses invalidate the control.
The second A read supplies the consecutive A/A noise observation; this
does not substitute for an independent day-to-day repeatability campaign.

ADR-0093 D9 thresholds stay unchanged: B/A write throughput at least 1.3
at c64 and 1.5 at c256; c256 p50 B/A at most 0.5; tiered/non-tiered c256
throughput at least 0.70; median filled-read throughput within ±2%, with
the A/A floor disclosed. Record p99/p99.9 beside throughput. Inspect raw
fallback categories: pin/ticket-cap fallbacks above 50% of eligible writes
or stale verdicts above 10% of resolutions trigger the ADR's falsifiers.
Do not substitute the older aggregate-fallback-per-total-SET key for the
eligible-write denominator. Pinned bytes, ticket bounds, all per-cell
tripwires and attribution must be reviewed from raw INFO.

The per-arm parity control is measured after that arm's tiered write leg;
device state is separated by the idle interval, not assumed identical.
Report generator CPU usage and the pinned-core utilization trace. A
saturated generator or red same-run tripwire invalidates a claim. The
default stays off unless all D9 conditions and correctness obligations
are met and the resulting disposition is recorded in the plan and ledger.

### DBSIZE — exact count with thousands of open tickets

`--s37-dbsize --s37-del-keys 12288 --s37-del-cycles 1`.
Each leg fills the namespace, pauses reconciliation, and overwrites each
fresh window. A disables shadow overwrite; B enables it. Before the timed
node-wide DBSIZE, B must have at least 70% of the window unverified and A
must have none. The exact 1,000,000-key result, one read per unverified
ticket, and zero unverified tickets afterward are mandatory. Verified
tickets awaiting settlement are disclosed separately in raw INFO.

Each cycle measures one drain followed by 32 empty-drain controls on the
same connection. Individual drain timings and their median are reported;
three drain samples per arm cannot support a meaningful p99.9 claim.
This is an informational latency row, with no invented performance gate.

**Preparation correction before the reference campaign:** the first smoke
stopped on a failed fill. The second, with a common profile and 40 s idle,
checked its first B drain but refused the second window (4,096 unverified
tickets versus the unchanged 70% coverage requirement). Unlike DEL,
DBSIZE preserves its winners and the subsequent window has a different
residency history. Each reference replicate therefore measures one window
on its own fresh database. Neither failed trial is used for performance
acceptance; the coverage rule remains unchanged.

## Run and record

Prepare the host so `inf-bench env-check` passes without overrides. If a
reboot or host setting change is needed, coordinate it with the operator;
this campaign does not perform either. Then:

```bash
# From the Rust repository root, after operator-controlled host preparation:
cargo build --locked --release -p infinityd -p inf-bench
./scripts/run-s37-reference.sh
```

The runner creates a fresh directory under `~/bench-data/s37/`; optionally
set `S37_CAMPAIGN_ROOT` to a fresh local output path. Inspect raw output
locally, then record exact revisions, commands, reference-box details,
results/spread, exit status and disposition in the owning plan/review and
claim ledgers. Do not commit reports, logs, profiles or database images.
Never relabel an overridden environment as reference. See
[validation and output policy](validation.md).
