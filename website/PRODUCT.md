# Product

## Register

brand

## Platform

web

## Users

Two audiences, equally primary: backend/infra engineers evaluating InfinityDB as a Redis-compatible durable store, and systems-programming followers (the HN / TigerBeetle-adjacent crowd) who follow the build for its engineering discipline. Both arrive skeptical of database marketing and fluent in the domain — they read compat matrices and fsync semantics for fun, and they punish overclaiming instantly.

## Product Purpose

The public website for InfinityDB: a static landing page, documentation set, and engineering blog, deployed to GitHub Pages from the monorepo. It exists to earn credible attention for a pre-1.0 database project. Success is a visitor starring and watching the repo to follow the milestone train.

## Positioning

Evidence-governed engineering. InfinityDB is built the way TigerBeetle and FoundationDB are built — determinism, DST, STOP gates, and a claim ledger that forbids any public number without an artifact. Every page reinforces that the discipline itself is the differentiator.

## Conversion & proof

- Primary CTA: star + watch the GitHub repo.
- Secondary CTA: read the blog ("The log is the database" is the best pitch for the discipline model).
- The line a visitor remembers after 10 seconds: **"The log is the database."**
- Belief ladder: (1) this is a real, serious engineering effort, not vaporware → (2) the discipline is genuinely unusual — laws, DST, STOP gates, claim ledger → (3) what ships today is honestly labeled and works → (4) therefore the big roadmap is credible → star + watch the milestone train.
- Proof on hand: the claim ledger itself (`site/_ledger-snapshot.md`; durability page cites Allowed rows C12–C16, C21 verbatim with artifacts), the first **Allowed measured rows on the landing page** — memory vs Redis in-run (C7), unpipelined throughput vs Redis (C5), the comparator-anchored cross-cell result (C5/ADR-0035), LFU parity (C8), cold-boot recovery (C14), checkpoint overhead (C16) — each in an evidence block or stat tile with its disclosures, the generated per-command compat matrix (`site/docs/compat.html`, rendered from the `inf-wire` registry so it cannot drift from the implementation), the CI ledger-copy tripwire (`scripts/check-ledger-copy.py`), and the blog (`why-vortex-failed.html` post-mortem + `the-log-is-the-database.html`). **Measured, not promised** is the stance: only Allowed/Narrowed rows appear, targets never do (master plan §18), and the landing page says out loud which rows are still absent and why — the designed absence remains part of the proof.

## Brand Personality

Ambitious, disciplined, warm. Big roadmap energy — one engine for the cache, the ledger, the queue, the document, and the vector — held accountable by visible discipline, and delivered in a voice that invites people along for the build rather than lecturing them. Honesty about what doesn't ship yet is a feature of the voice, not a disclaimer: milestone labels, "IN DEV" badges, and loss-window tables are worn openly.

## Anti-references

- Benchmark-war database marketing: hype multipliers, cherry-picked charts, "fastest database" claims — the exact opposite of the ledger policy. Comparative bars exist on this site only as in-run, ledger-Allowed measurements with the competitor's context disclosed.
- Template SaaS landing pages: logo walls, pricing-tier cards, marketing-speak, decoration without meaning. (The dual-lamp system sanctions exactly one gradient blend — hero phrase, bar fills, featured wash — as brand voice, not as SaaS wallpaper; DESIGN.md's Two Lamps Rule governs it.)
- Academic / plain-text austerity: the honesty must not read as a LaTeX paper dump with no craft; discipline and polish coexist.

## Design Principles

1. **Practice what you preach** — the site is itself an artifact of the discipline it sells: generated pages that cannot drift, CI tripwires on copy, ledger-cited numbers or none at all.
2. **Honesty is the aesthetic** — milestone labels, loss windows, and "not shipped yet" callouts are designed as first-class brand elements, not buried fine print.
3. **Show the machinery** — the compat matrix, the roadmap train, and the ledger are the marketing; let readers inspect rather than asking them to trust.
4. **Ambition with receipts** — the roadmap's breadth is stated boldly, always adjacent to the evidence that makes it credible.
5. **Warm, not salesy** — write to a peer following the build, never at a prospect being converted.

## Accessibility & Inclusion

Best-effort defaults, no formal WCAG target: sensible contrast, semantic HTML, keyboard-reachable nav, and reduced-motion respect where animation exists.
