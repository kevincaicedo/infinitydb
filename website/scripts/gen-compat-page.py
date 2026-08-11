#!/usr/bin/env python3
"""Render the InfinityDB Redis compatibility matrix as a static HTML page.

The compat page is NEVER hand-written (project law L8: compatibility is
staged and honest). This script is the contract: it converts the repo's
generated artifact `infinitydb/docs/compat-matrix.md` (itself rendered from
the inf-wire command registry + the oracle-diff corpus by
`tests/compat/src/matrixgen.rs`, with a CI staleness gate) into
`site/docs/compat.html`. The generated page is committed; CI regenerates it
and fails if it drifted.

Usage:
    python3 scripts/gen-compat-page.py \
        --matrix infinitydb/docs/compat-matrix.md \
        --out site/docs/compat.html

stdlib only. No third-party dependencies.
"""

import argparse
import html
import re
import sys
from datetime import date
from pathlib import Path

STATUS_ORDER = ["full", "partial", "stub", "extension", "internal"]
STATUS_CLASS = {
    "full": "st-full",
    "partial": "st-partial",
    "stub": "st-stub",
    "extension": "st-ext",
    "internal": "st-int",
}


def md_inline(text: str) -> str:
    """Escape HTML, then apply the two inline markdown forms the artifact
    uses: `code` and **bold**."""
    out = html.escape(text, quote=False)
    out = re.sub(r"`([^`]+)`", r"<code>\1</code>", out)
    out = re.sub(r"\*\*([^*]+)\*\*", r"<strong>\1</strong>", out)
    return out


def parse_matrix(md: str) -> dict:
    lines = md.splitlines()
    data = {
        "preamble": [],   # blockquote + oracle paragraphs before "## Commands"
        "corpus": "",
        "surface": "",
        "rows": [],       # dicts: command,status,since,flags,arity,cases,notes
        "deviations": [], # (command, [bullets])
    }

    # --- head section ---
    i = 0
    while i < len(lines) and not lines[i].startswith("## Commands"):
        line = lines[i]
        if line.startswith("**Corpus:**"):
            data["corpus"] = line.replace("**Corpus:**", "").strip().rstrip(".")
        elif line.startswith("**Surface:**"):
            data["surface"] = line.replace("**Surface:**", "").strip().rstrip(".")
        elif line.startswith(">"):
            data["preamble"].append(line.lstrip("> ").strip())
        i += 1

    # --- command table ---
    while i < len(lines) and not lines[i].startswith("| Command"):
        i += 1
    if i >= len(lines):
        sys.exit("error: command table not found in matrix artifact")
    i += 2  # skip header + separator
    while i < len(lines) and lines[i].startswith("|"):
        cells = [c.strip() for c in lines[i].strip().strip("|").split("|")]
        if len(cells) >= 7:
            cmd = cells[0].strip("`").strip()
            data["rows"].append({
                "command": cmd,
                "status": cells[1],
                "since": cells[2],
                "flags": cells[3],
                "arity": cells[4],
                "cases": cells[5],
                "notes": cells[6],
            })
        i += 1

    # --- deviations ---
    while i < len(lines) and not lines[i].startswith("## Documented deviations"):
        i += 1
    current = None
    for line in lines[i:]:
        m = re.match(r"^###\s+`?([^`]+)`?\s*$", line)
        if m:
            current = (m.group(1).strip(), [])
            data["deviations"].append(current)
        elif line.startswith("- ") and current is not None:
            current[1].append(line[2:].strip())
    return data


PAGE_TEMPLATE = """<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Redis compatibility matrix — InfinityDB Docs</title>
<meta name="description" content="Per-command Redis compatibility for InfinityDB, byte-diff-verified against Redis 8.0.5. Rendered from the generated repository artifact.">
<link rel="icon" href="data:image/svg+xml,%3Csvg%20xmlns='http://www.w3.org/2000/svg'%20viewBox='0%200%2034%2018'%3E%3Ccircle%20cx='10'%20cy='9'%20r='7'%20fill='none'%20stroke='%237c5cff'%20stroke-width='2.4'/%3E%3Ccircle%20cx='24'%20cy='9'%20r='7'%20fill='none'%20stroke='%233ee6c4'%20stroke-width='2.4'/%3E%3C/svg%3E">
<link rel="preconnect" href="https://fonts.googleapis.com">
<link rel="preconnect" href="https://fonts.gstatic.com" crossorigin>
<link href="https://fonts.googleapis.com/css2?family=Archivo:wght@500;600;700;800&family=JetBrains+Mono:wght@400;500;700&display=swap" rel="stylesheet">
<link rel="stylesheet" href="../assets/site.css">
<style>
.stat-row {{ display: grid; grid-template-columns: repeat(auto-fit, minmax(120px, 1fr)); gap: 12px; margin: 28px 0; }}
.stat-row .stat {{ background: var(--bg-raise); border: 1px solid var(--border-mid); border-radius: 10px; padding: 14px 16px; }}
.stat-row .v {{ font: 700 24px/1.1 var(--mono); letter-spacing: -0.01em; color: var(--text); }}
.stat-row .l {{ font: 500 9.5px/1.4 var(--mono); letter-spacing: 0.1em; color: var(--text-faint); text-transform: uppercase; margin-top: 6px; }}
.st {{ display: inline-block; font: 500 10px/1 var(--mono); letter-spacing: 0.06em; border-radius: 99px; padding: 4px 9px; }}
.st-full {{ color: var(--teal); border: 1px solid var(--teal-dim); }}
.st-partial {{ color: var(--warn); border: 1px solid rgba(255, 196, 92, 0.4); }}
.st-stub {{ color: var(--text-faint); border: 1px solid var(--border-strong); }}
.st-ext {{ color: var(--violet-soft); border: 1px solid rgba(124, 92, 255, 0.4); }}
.st-int {{ color: var(--text-faint); border: 1px dashed var(--border-strong); }}
.filters {{ display: flex; gap: 8px; flex-wrap: wrap; margin: 18px 0 6px; }}
.filters button {{
  font: 500 11px/1 var(--mono); letter-spacing: 0.06em; cursor: pointer;
  background: var(--bg-raise); color: var(--text-muted);
  border: 1px solid var(--border-strong); border-radius: 99px; padding: 8px 14px;
}}
.filters button:hover {{ color: var(--teal); border-color: var(--teal-dim); }}
.filters button[aria-pressed="true"] {{ color: var(--teal); border-color: var(--teal-dim); background: var(--teal-wash); }}
.dev h3 {{ font-size: 15px; margin: 26px 0 8px; font-family: var(--mono); font-weight: 500; color: var(--teal); }}
.dev ul {{ margin: 0 0 0 2px; padding-left: 20px; }}
.dev li {{ font-size: 13.5px; color: var(--text-muted); }}
#matrix td:first-child {{ color: var(--text); }}
</style>
</head>
<body>

<nav class="nav">
  <div class="nav-inner">
    <a href="../index.html" class="nav-brand">
      <svg class="logo" width="34" height="18" viewBox="0 0 34 18" aria-hidden="true">
        <circle cx="10" cy="9" r="7" fill="none" stroke="#7c5cff" stroke-width="2.4"/>
        <circle cx="24" cy="9" r="7" fill="none" stroke="#3ee6c4" stroke-width="2.4"/>
      </svg>
      <span class="mark">infinity<b>DB</b></span>
      <span class="nav-badge neutral">DOCS</span>
    </a>
    <span class="nav-spacer"></span>
    <button class="nav-toggle" aria-label="Toggle navigation" aria-expanded="false">&#9776;</button>
    <div class="nav-links">
      <a href="../index.html">HOME</a>
      <a href="../blog/index.html">BLOG</a>
      <a href="roadmap.html">ROADMAP</a>
      <a class="nav-cta" href="https://github.com/">&#9733; GITHUB</a>
    </div>
  </div>
</nav>

<div class="docs-shell">
  <aside class="docs-side">
    <p class="side-h">Getting started</p>
    <a class="side-link" href="index.html">Docs home</a>
    <a class="side-link" href="quickstart.html">Quickstart</a>
    <p class="side-h">Concepts</p>
    <a class="side-link" href="architecture.html">Architecture</a>
    <a class="side-link" href="durability.html">Namespaces &amp; durability</a>
    <p class="side-h">Operations</p>
    <a class="side-link" href="deployment.html">Deployment</a>
    <a class="side-link" href="operations.html">Operations</a>
    <p class="side-h">Evidence</p>
    <a class="side-link" href="benchmarks.html">Benchmarks &amp; evidence</a>
    <a class="side-link" href="../evidence/inf-compare.html">Comparative report</a>
    <p class="side-h">Reference</p>
    <a class="side-link" href="compat.html" aria-current="page">Command matrix</a>
    <a class="side-link" href="roadmap.html">Roadmap</a>
  </aside>

  <main class="docs-main" style="max-width: none;">
    <article>
      <p class="doc-kicker">Reference</p>
  <h1 class="page-title">Redis compatibility matrix</h1>
  <p class="lede">Compatibility is declared per command and verified byte-for-byte against a real Redis oracle in CI &mdash; any new deviation fails the build until it is documented. This page is <strong>rendered from the repository&rsquo;s generated artifact</strong>, never written by hand, so it cannot drift from the implementation.</p>

  <div class="callout">
    <span class="callout-tag"><strong>Generated page &mdash; do not edit.</strong></span>
    Rendered from <code>infinitydb/docs/compat-matrix.md</code> (itself generated from the <code>inf-wire</code> command registry and the oracle-diff corpus by <code>tests/compat/src/matrixgen.rs</code>, with a CI staleness gate) by <code>scripts/gen-compat-page.py</code> on {gen_date}. {preamble}
  </div>

  <div class="stat-row">
{stats}
  </div>
  <p class="mono-note">CORPUS: {corpus_html} &middot; SURFACE: {surface_html}</p>

  <h2 style="margin-top:40px">Status vocabulary</h2>
  <ul>
    <li><span class="st st-full">full</span>&ensp;behavior-contract equivalent (recorded deviations are representational: ordering, identity payloads, opaque cursors/art)</li>
    <li><span class="st st-partial">partial</span>&ensp;a documented semantic difference exists</li>
    <li><span class="st st-stub">stub</span>&ensp;accepted but inert</li>
    <li><span class="st st-ext">extension</span>&ensp;<code>INF.*</code> surface unknown to Redis</li>
    <li><span class="st st-int">internal</span>&ensp;fabric program primitives, not a client surface</li>
  </ul>

  <h2>Commands</h2>
  <div class="filters" role="group" aria-label="Filter by status">
    <button data-f="all" aria-pressed="true">ALL ({total})</button>
{filter_buttons}
  </div>
  <div class="table-wrap">
    <table class="data" id="matrix">
      <thead>
        <tr><th>Command</th><th>Status</th><th>Since</th><th>Flags</th><th>Arity</th><th>Cases</th><th>Notes</th></tr>
      </thead>
      <tbody>
{rows}
      </tbody>
    </table>
  </div>

  <h2>Documented deviations (the allowlist, verbatim)</h2>
  <p class="muted">Each entry is a justification from the diff corpus: the candidate must still produce well-formed RESP for these cases, but the bytes differ from the oracle by design.</p>
  <div class="dev">
{deviations}
  </div>

  <div class="doc-footer-nav">
    <a href="architecture.html">&larr; Architecture</a>
    <a href="roadmap.html">Roadmap &rarr;</a>
  </div>
    </article>
  </main>
</div>

<footer class="footer">
  <div class="footer-inner">
    <span class="mark">&copy; 2026 INFINITYDB &mdash; APACHE 2.0 &middot; RUST &middot; <b>infinityd</b></span>
    <div class="footer-links">
      <a href="index.html">DOCS</a>
      <a href="../blog/index.html">BLOG</a>
      <a href="roadmap.html">ROADMAP</a>
      <a href="https://github.com/">GITHUB &#8599;</a>
    </div>
  </div>
</footer>

<script>
(function () {{
  var t = document.querySelector('.nav-toggle');
  var l = document.querySelector('.nav-links');
  if (t && l) t.addEventListener('click', function () {{
    var open = l.classList.toggle('open');
    t.setAttribute('aria-expanded', open ? 'true' : 'false');
  }});
  var buttons = document.querySelectorAll('.filters button');
  var rows = document.querySelectorAll('#matrix tbody tr');
  buttons.forEach(function (b) {{
    b.addEventListener('click', function () {{
      buttons.forEach(function (x) {{ x.setAttribute('aria-pressed', 'false'); }});
      b.setAttribute('aria-pressed', 'true');
      var f = b.getAttribute('data-f');
      rows.forEach(function (r) {{
        r.style.display = (f === 'all' || r.getAttribute('data-status') === f) ? '' : 'none';
      }});
    }});
  }});
}})();
</script>
</body>
</html>
"""


def render(data: dict, gen_date: str) -> str:
    counts = {}
    for r in data["rows"]:
        counts[r["status"]] = counts.get(r["status"], 0) + 1
    total = len(data["rows"])

    stats_parts = [
        '    <div class="stat"><div class="v">{}</div><div class="l">commands declared</div></div>'.format(total)
    ]
    for st in STATUS_ORDER:
        if counts.get(st):
            stats_parts.append(
                '    <div class="stat"><div class="v">{}</div><div class="l">{}</div></div>'.format(counts[st], html.escape(st))
            )
    stats = "\n".join(stats_parts)

    filter_buttons = "\n".join(
        '    <button data-f="{0}" aria-pressed="false">{1} ({2})</button>'.format(
            html.escape(st), html.escape(st.upper()), counts[st]
        )
        for st in STATUS_ORDER if counts.get(st)
    )

    row_html = []
    for r in data["rows"]:
        cls = STATUS_CLASS.get(r["status"], "st-stub")
        row_html.append(
            "        <tr data-status=\"{st}\"><td><code>{cmd}</code></td>"
            "<td><span class=\"st {cls}\">{st}</span></td>"
            "<td>{since}</td><td class=\"faint\">{flags}</td>"
            "<td>{arity}</td><td>{cases}</td><td>{notes}</td></tr>".format(
                st=html.escape(r["status"]),
                cmd=html.escape(r["command"]),
                cls=cls,
                since=html.escape(r["since"]),
                flags=html.escape(r["flags"]) or "&mdash;",
                arity=html.escape(r["arity"]),
                cases=html.escape(r["cases"]),
                notes=md_inline(r["notes"]) if r["notes"] else "",
            )
        )
    rows = "\n".join(row_html)

    dev_html = []
    for cmd, bullets in data["deviations"]:
        dev_html.append("    <h3><code>{}</code></h3>".format(html.escape(cmd)))
        dev_html.append("    <ul>")
        for b in bullets:
            dev_html.append("      <li>{}</li>".format(md_inline(b)))
        dev_html.append("    </ul>")
    deviations = "\n".join(dev_html)

    # The artifact's own blockquote repeats provenance we already state;
    # keep only its upstream-regeneration instructions.
    joined = " ".join(p for p in data["preamble"] if p)
    m = re.search(r"(Regenerate:.*)$", joined)
    preamble = md_inline(m.group(1)).replace("Regenerate:", "Upstream regenerate:", 1) if m else ""

    return PAGE_TEMPLATE.format(
        gen_date=gen_date,
        preamble=preamble,
        stats=stats,
        corpus_html=md_inline(data["corpus"]),
        surface_html=md_inline(data["surface"]),
        total=total,
        filter_buttons=filter_buttons,
        rows=rows,
        deviations=deviations,
    )


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--matrix", required=True, help="path to infinitydb/docs/compat-matrix.md")
    ap.add_argument("--out", required=True, help="path to write site/docs/compat.html")
    ap.add_argument("--date", default=None, help="generation date stamp (default: today, UTC)")
    args = ap.parse_args()

    md = Path(args.matrix).read_text(encoding="utf-8")
    data = parse_matrix(md)
    if not data["rows"]:
        sys.exit("error: no command rows parsed — artifact format changed?")
    page = render(data, args.date or date.today().isoformat())
    Path(args.out).parent.mkdir(parents=True, exist_ok=True)
    Path(args.out).write_text(page, encoding="utf-8")
    print("wrote {} ({} commands, {} deviation groups)".format(args.out, len(data["rows"]), len(data["deviations"])))


if __name__ == "__main__":
    main()
