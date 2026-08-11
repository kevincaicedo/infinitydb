#!/usr/bin/env python3
"""Ledger-copy check (project law L10, mechanized for the website).

Scans the site's HTML for number-bearing performance-claim-shaped tokens
(multipliers like "2.7x", rates like "300k ops/s", bandwidth, latencies,
sizes, percentages, pNN comparisons) and fails if any such token is not in
the allowlist. The allowlist (`scripts/ledger-allowed-numbers.txt`) is
maintained from the claim ledger's `Allowed` rows: a number may only be
allowlisted if an Allowed ledger row covers it (or if it is demonstrably a
non-claim, e.g. a config example — say so in the line comment).

This is deliberately pragmatic, not clever: it cannot judge *wording*, so
the release-manager checklist in docs/claim-ledger.md still applies. What
it guarantees mechanically is that no unreviewed performance number lands
on the site.

Ledger sources, in order of preference:
  1. --ledger <path>   (the live docs/claim-ledger.md, when the site is
                        built inside the monorepo)
  2. site/_ledger-snapshot.md (a committed snapshot, so the check still
                        runs if the site is split into its own repo; the
                        tradeoff — snapshots can go stale — is documented
                        in the website README)
The ledger is used for a soft cross-check (warn if an allowlisted token
does not appear in the ledger text); the hard gate is the allowlist.

Usage:
    python3 scripts/check-ledger-copy.py --site site \
        [--ledger docs/claim-ledger.md] \
        [--allowlist scripts/ledger-allowed-numbers.txt] \
        [--print-tokens]

Exit codes: 0 = clean, 1 = violations found, 2 = usage/config error.
stdlib only.
"""

import argparse
import html
import re
import sys
from pathlib import Path

# ---------------------------------------------------------------- patterns

NUM = r"(\d[\d,]*(?:\.\d+)?)"

# Each pattern yields (number, unit-tag). Order matters: more specific first.
PATTERNS = [
    # 2.76x / 1.44 × (comparative multiplier)
    (re.compile(NUM + r"\s*[x×]\b", re.I), "x"),
    # rates: 300k ops/s, 2.5M msgs/s, 128k w/s, 10k QPS ...
    (re.compile(NUM + r"\s*([kKmMgG]?)\s*(ops/s|msgs?/s|writes?/s|w/s|qps|req/s|txns?/s|deliveries/s|vec/s|entries/s)", re.I), "rate"),
    # bandwidth: 1 GB/s, 1.07 GiB/s
    (re.compile(NUM + r"\s*(gb/s|gib/s|mb/s|mib/s|kb/s)", re.I), "bw"),
    # latency / durations: 975 us, 2 ms, 9.8 s, 10 seconds
    (re.compile(NUM + r"\s*(ms|µs|us|ns|s|sec|secs|second|seconds)\b", re.I), "time"),
    # sizes: 10 GB, 14.4 MiB, 30 MB (not followed by /s — bandwidth handled above)
    (re.compile(NUM + r"\s*(gb|gib|mb|mib|kb|kib|tb)\b(?!/s)", re.I), "size"),
    # percentages: 1.33%, < 10 %
    (re.compile(NUM + r"\s*%", re.I), "pct"),
    # pNN comparisons: p99.9 < 2 ms (number captured; unit via the time pattern too)
    (re.compile(r"p\d{2}(?:\.\d+)?\s*(?:[<>≤≥≈=~]|&lt;|&gt;)\s*" + NUM, re.I), "pcmp"),
]

UNIT_NORMALIZE = {
    "sec": "s", "secs": "s", "second": "s", "seconds": "s",
    "µs": "us",
    "msg/s": "msgs/s", "write/s": "writes/s", "txn/s": "txns/s",
}


def canonical(num: str, unit: str) -> str:
    num = num.rstrip(".,").replace(",", "")
    unit = UNIT_NORMALIZE.get(unit.lower(), unit.lower())
    return f"{num} {unit}".strip()


def extract_tokens(text: str):
    """Yield (canonical_token, snippet) for each perf-shaped number."""
    for pat, kind in PATTERNS:
        for m in pat.finditer(text):
            if kind == "x":
                token = canonical(m.group(1), "x")
            elif kind == "rate":
                token = canonical(m.group(1) + m.group(2).lower(), m.group(3))
            elif kind in ("bw", "time", "size"):
                token = canonical(m.group(1), m.group(2))
            elif kind == "pct":
                token = canonical(m.group(1), "%")
            else:  # pcmp
                token = canonical(m.group(1), "p-cmp")
            start = max(0, m.start() - 40)
            snippet = re.sub(r"\s+", " ", text[start:m.end() + 40]).strip()
            yield token, snippet


# ------------------------------------------------------------- html -> text

TAG_STRIP = re.compile(r"<(script|style)\b.*?</\1>", re.S | re.I)
COMMENT_STRIP = re.compile(r"<!--.*?-->", re.S)
TAGS = re.compile(r"<[^>]+>")


def visible_text(html_src: str) -> str:
    txt = TAG_STRIP.sub(" ", html_src)
    txt = COMMENT_STRIP.sub(" ", txt)
    txt = TAGS.sub(" ", txt)
    return html.unescape(txt)


# ---------------------------------------------------------------- allowlist

def load_allowlist(path: Path) -> set:
    if not path.exists():
        sys.exit(f"error: allowlist not found: {path}")
    tokens = set()
    for raw in path.read_text(encoding="utf-8").splitlines():
        line = raw.split("#", 1)[0].strip()
        if line:
            tokens.add(line.lower())
    return tokens


def ledger_allowed_text(path: Path) -> str:
    """Concatenated text of rows whose status cell contains Allowed or
    Narrowed (the narrowed wording is the allowed wording)."""
    out = []
    for line in path.read_text(encoding="utf-8").splitlines():
        if line.startswith("|") and ("**Allowed**" in line or "**Narrowed**" in line):
            out.append(line)
    return "\n".join(out)


# --------------------------------------------------------------------- main

def main() -> int:
    ap = argparse.ArgumentParser(description="Fail if site HTML carries a perf number absent from the ledger allowlist.")
    ap.add_argument("--site", default="site", help="site directory to scan (default: site)")
    ap.add_argument("--allowlist", default="scripts/ledger-allowed-numbers.txt")
    ap.add_argument("--ledger", default=None, help="path to docs/claim-ledger.md (optional; falls back to <site>/_ledger-snapshot.md)")
    ap.add_argument("--print-tokens", action="store_true", help="print every token found (debugging)")
    args = ap.parse_args()

    site = Path(args.site)
    if not site.is_dir():
        print(f"error: site directory not found: {site}", file=sys.stderr)
        return 2
    allow = load_allowlist(Path(args.allowlist))

    ledger_path = Path(args.ledger) if args.ledger else site / "_ledger-snapshot.md"
    ledger_text = ""
    if ledger_path.exists():
        ledger_text = ledger_allowed_text(ledger_path).lower()
        print(f"ledger source: {ledger_path}")
    else:
        print(f"warning: no ledger available ({ledger_path} missing) — soft cross-check skipped", file=sys.stderr)

    violations = []
    found_any = []
    for page in sorted(site.rglob("*.html")):
        # site/evidence/ holds GENERATED verbatim renders of citation-grade
        # campaign artifacts (gen-compare-page.py, which hard-refuses any
        # report whose tier banner is not binding). Those pages ARE the
        # artifacts the ledger rows cite — the copy gate exists to stop
        # unreviewed numbers in *prose*, not to re-review the evidence
        # itself. Hand-written pages must never live under evidence/.
        if "evidence" in page.relative_to(site).parts:
            continue
        text = visible_text(page.read_text(encoding="utf-8"))
        for token, snippet in extract_tokens(text):
            found_any.append((page, token, snippet))
            if token.lower() not in allow:
                violations.append((page, token, snippet))

    if args.print_tokens:
        for page, token, snippet in found_any:
            print(f"  [{token}] {page}: …{snippet}…")

    # soft cross-check: allowlisted tokens should trace to the ledger
    if ledger_text:
        for entry in sorted(allow):
            num = entry.split(" ")[0]
            if num and num not in ledger_text:
                print(f"warning: allowlist entry '{entry}' — number '{num}' not found in any "
                      f"Allowed/Narrowed ledger row; confirm it is a documented non-claim", file=sys.stderr)

    if violations:
        print(f"\nFAIL: {len(violations)} performance-shaped number(s) not covered by an "
              f"Allowed claim-ledger row / the allowlist:\n", file=sys.stderr)
        for page, token, snippet in violations:
            print(f"  {page}\n    token:   {token}\n    context: …{snippet}…\n", file=sys.stderr)
        print("Either remove the number, or (only if a ledger row Allows it) add the token to "
              f"{args.allowlist} with a comment citing the row.", file=sys.stderr)
        return 1

    print(f"OK: {len(found_any)} perf-shaped token(s) scanned across "
          f"{len(list(site.rglob('*.html')))} pages — all covered.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
