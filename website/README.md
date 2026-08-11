# InfinityDB website v2 (design refresh, 2026-07-17)

Fully static, multi-page project website for GitHub Pages. Plain HTML/CSS +
minimal vanilla JS — no framework, no build step for pages. The only
generated page is the compat matrix (see below). v2 ports the approved
dual-accent design prototypes `assests/Landing.dc.html` / `Docs.dc.html` /
`Blog.dc.html` (violet #7c5cff + teal #3ee6c4 on #06070d — see DESIGN.md,
"Two Lamps on Near-Black"), with all `sc-if`/`x-dc`/`support.js`
scaffolding replaced by vanilla equivalents (copy button,
IntersectionObserver bar triggers, nav toggle).

## Layout

```
site/                          ← the deployable root (upload this to Pages)
  index.html                   landing page
  assets/site.css              the one shared stylesheet
  _ledger-snapshot.md          committed snapshot of docs/claim-ledger.md (CI fallback)
  docs/
    index.html                 docs hub + claims/evidence explainer
    quickstart.html            source build + Docker-from-repo + seccomp note
                               + client snippets (redis-py/node-redis/go-redis/Lettuce, M2.5-S06)
    durability.html            namespaces, durability classes, loss windows, recovery
    deployment.html            docker/systemd/seccomp, data-dir layout, flags, upgrades (M2.5-S06)
    operations.html            -LOADING, refusal taxonomy, INFO persistence, alpha limits (M2.5-S06)
    architecture.html          the internals tour with animated diagrams
                               (cell, fabric, group commit, M4 tiering)
    benchmarks.html            methodology + every Allowed number w/ artifact
    compat.html                GENERATED — do not edit (see below)
    roadmap.html               the milestone train, ADR reorders named
  evidence/
    inf-compare.html           GENERATED — the binding comparative campaign
                               report, rendered verbatim (see below)
  blog/
    index.html
    the-log-is-the-database.html   featured post (discipline model)
scripts/
  gen-compat-page.py           renders compat.html from the repo artifact
  gen-compare-page.py          renders evidence/inf-compare.html from a
                               BINDING inf-compare report (refuses dev-tier)
  check-ledger-copy.py         CI: no perf number without an Allowed ledger row
                               (skips site/evidence/ — generated artifact renders)
  ledger-allowed-numbers.txt   the allowlist (regenerated from Allowed rows)
```

The Vortex post-mortem article and comparison bar were retired 2026-08-10
(owner decision): no vortex content on the site; the predecessor survives
only as unnamed context in the discipline post.

Placement (as landed, M2.5-S05): this directory is `website/` in the
InfinityDB repo; the deploy workflow is `.github/workflows/pages.yml` with
paths already adjusted (`website/site`, `website/scripts`,
`docs/compat-matrix.md`). The ledger of record (`docs/claim-ledger.md`)
lives in the outer planning repo, so CI checks against the committed
snapshot `site/_ledger-snapshot.md` — refresh it whenever site copy or the
ledger changes (release-manager checklist step).

## Preview locally

```bash
cd site && python3 -m http.server 8000
# open http://localhost:8000
```

Everything is self-contained except the Google Fonts links (Archivo +
JetBrains Mono); the site degrades gracefully to system fonts offline.

## The compat page is generated — never edit it

`site/docs/compat.html` is rendered from the repo's own generated artifact
`infinitydb/docs/compat-matrix.md` (which is itself rendered from the
`inf-wire` command registry by `tests/compat/src/matrixgen.rs`, with its own
CI staleness gate). The chain keeps the website incapable of drifting from
the implementation (law L8). Regenerate after the matrix changes:

```bash
python3 scripts/gen-compat-page.py \
  --matrix infinitydb/docs/compat-matrix.md \
  --out site/docs/compat.html
```

Commit the regenerated page. The workflow regenerates it and fails the
build if the committed page is stale (only the date stamp may differ).

## The ledger-copy check

Project law L10: no number in public copy without an **Allowed** row in
`docs/claim-ledger.md`. `scripts/check-ledger-copy.py` mechanizes the
website half of that rule: it strips every HTML page to visible text,
extracts performance-claim-shaped tokens (multipliers `2.7x`, rates
`ops/s`, bandwidth, latencies, sizes, percentages, `p99 < N` comparisons)
and fails unless each token is in `scripts/ledger-allowed-numbers.txt`.

```bash
python3 scripts/check-ledger-copy.py --site site \
  --ledger docs/claim-ledger.md \
  --allowlist scripts/ledger-allowed-numbers.txt --print-tokens
```

- The allowlist is maintained **from the ledger's Allowed rows** — every
  entry's comment names its row (or documents why it is a non-claim, e.g.
  the `MAXMEMORY 16gb` config example). Review it whenever the ledger
  changes.
- Ledger source: pass `--ledger` when building inside the monorepo (the
  workflow does). If the site is ever split into its own repo, the
  committed snapshot `site/_ledger-snapshot.md` is used instead —
  **tradeoff**: a snapshot can go stale relative to the live ledger, which
  is why the workflow refreshes it from `docs/claim-ledger.md` on every
  deploy and the snapshot is only a fallback.
- The check is a tripwire, not a replacement for the release-manager
  checklist in the ledger (it checks numbers, not wording).

Current numbers on the site and their coverage: the landing page shows
the first Allowed measured rows — **C5** (unpipelined 3.21× Redis +
the 1.72× Dragonfly cross-cell anchor, disclosures in the mono footnote),
**C7** (0.61× RSS), **C8** (96.19% LFU parity), **C12** (10k-seed sweep),
**C14** (9.8 s cold boot), **C16** (14.4 MiB checkpoint overhead) — in
evidence blocks and stat tiles; `docs/benchmarks.html` carries the full
Allowed set including the M3 document rows (**C21**, **C24–C27**) beside
the methodology (**C19**'s everysec range came OFF the page 2026-08-11 —
its S24 re-read did not reproduce and the row reverted to
Evidence-pending; the page's absence list tells that story); the durability
page cites **C12–C16, C21** verbatim with artifacts; the blog cites
**C3, C4, C12**. Deployment/operations carry only config non-claims
(`256 MiB` defaults, `16 MiB` bound). `evidence/inf-compare.html` is the
generated verbatim render of the binding comparative report (excluded
from the copy check by design — it IS the artifact; the generator
refuses non-binding reports). Pipelined peaks, `always`-mode write
rates, absolute tail-latency claims, and all tiered-storage numbers
remain absent — Narrowed/Evidence-pending rows never render, and both
the landing's "what you don't see here" callout and the benchmarks
page's absence list say so explicitly.

## Deploying to GitHub Pages

1. Copy `pages.yml` to `.github/workflows/pages.yml` (adjust paths if you
   relocated `site/`/`scripts/`).
2. In the GitHub repo: **Settings → Pages → Build and deployment → Source:
   GitHub Actions.**
3. Push to `main` (or run the workflow manually). The workflow runs the
   compat staleness gate + the ledger check, then deploys `site/` via
   `actions/upload-pages-artifact` + `actions/deploy-pages`.
4. Optional custom domain: Settings → Pages → Custom domain, then add a
   `site/CNAME` file containing the domain so deploys keep it.

## TODO before going live (fill these in)

- [ ] **Real GitHub repo URL** — every `href="https://github.com/"` in the
  HTML is a placeholder (nav "GitHub" button, hero/CTA "Star on GitHub",
  footer links, the architecture page's repo links). Search-and-replace
  `https://github.com/` → `https://github.com/<org>/<repo>` (and deep links
  like `.../blob/main/infinitydb/ARCHITECTURE.md` where appropriate).
- [ ] Enable GitHub Pages as described above; verify the deployed URL.
- [ ] Custom domain, if any (`site/CNAME` + DNS).
- [ ] S05 AC: verify the quickstart cold on a clean machine (source build +
  Docker-from-repo path), per the milestone plan.
- [ ] When `v0.4.0-alpha` actually tags: update the quickstart's "no
  published binaries yet" callout to point at the release artifacts,
  re-run the ledger check against the release's re-validated ledger, and
  regenerate `evidence/inf-compare.html` from the v0.4.0 campaign's
  binding report once its rows are signed (S25 release checklist).

## Honesty invariants baked into the copy (keep them when editing)

- Shipping today = Redis-compatible in-memory cache + durable KV
  namespaces + the built-and-verified JSON document plane (M3 · verdict
  reached, tags at v0.4.0-alpha); tiered storage is M4 · IN DEV.
  Queries/collections/streams/vectors/compute/HA are roadmap items and
  every mention carries its milestone label — including terminal
  commands and diagram nodes.
- Version badge = `v0.4.0-alpha · IN DEV` (ADR-0067, 2026-08-06:
  `v0.3.0-alpha.1` retired unused; the first public tag is M4's
  `v0.4.0-alpha`, shipping M3 + M4 together, release act in M4-S25;
  nothing has been tagged yet).
- The roadmap page mirrors master plan §21–22 including the
  ADR-0023/0024 reorders, M4.5, and the ADR-0048 → ADR-0067 first-tag
  moves; the design prototypes' stale train (old numbering, "M1 · NOW")
  was corrected during the port, not reproduced.
- The Docker quickstart builds from the repo and keeps the io_uring seccomp
  requirement front and center.
- The `wait-replica` durability row is labeled M9 (not shipped).
