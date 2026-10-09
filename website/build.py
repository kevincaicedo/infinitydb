#!/usr/bin/env python3
"""Build the InfinityDB website: `src/` + `docs/compat-matrix.md` -> `site/`.

The site is static HTML, committed under `site/` and deployed as-is. Every
fact that more than one page shows lives here once: the milestone train and
the current milestone, the docs navigation, the post list, the pixel mark,
Moss, the figures and the page chrome. Content lives in `src/` as HTML
fragments with a front-matter comment.

The compatibility page is rendered from the repository's generated
artifact `docs/compat-matrix.md` and is never written by hand (law L8).

Usage, from the repository root:
    python3 website/build.py           # write site/
    python3 website/build.py --check   # exit 1 if site/ is stale

The output is a pure function of the inputs: no dates, no randomness that
is not seeded, so `--check` is a byte comparison (law L7).

stdlib only.
"""

import argparse
import datetime
import html
import json
import math
import re
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
SRC = HERE / "src"
OUT = HERE / "site"
MATRIX = HERE.parent / "docs" / "compat-matrix.md"

REPO = "https://github.com/kevincaicedo/infinitydb"
SITE_URL = "https://kevincaicedo.github.io/infinitydb/"
FONTS = (
    "https://fonts.googleapis.com/css2?family=Doto:wght@700;900"
    "&family=Geist+Mono:wght@400;500&family=Geist:wght@400;500;600&display=swap"
)

# ---------------------------------------------------------------------------
# The milestone train. NOW is the one place the current milestone is set:
# the announcement bar, every alpha badge, the roadmap and the docs tags
# follow it.

NOW = "M4"

# Big milestones only: public copy names a phase by what it does, never by a
# dot milestone (the public-docs gate, ADR-0166).
TRAIN = [
    ("M0", "Skeleton", "", "proving the architecture end to end"),
    ("M1", "Cache core", "", "a Redis-compatible cache core"),
    ("M2", "Durability", "", "the log, checkpoints and recovery"),
    ("M3", "JSON documents", "", "JSON documents as first-class values"),
    ("M4", "Tiered storage", "first public tag", "datasets beyond RAM, indexes and query"),
    ("M5", "Data types", "", "hashes, lists, sets and sorted sets"),
    ("M6", "Transactions", "", "transactions and scripting"),
    ("M7", "Streams & queues", "first beta", "streams, queues and the first beta"),
    ("M8", "Vectors", "", "vector search"),
    ("M9", "Replication & HA", "", "replication and high availability"),
    ("M10", "Compute & embedded", "", "WASM reducers and embedded mode"),
    ("M11", "GA hardening", "1.0.0", "hardening for 1.0.0"),
]
TRAIN_SHORT_TAG = {"first public tag": "first tag"}

NOW_INDEX = next(i for i, m in enumerate(TRAIN) if m[0] == NOW)
NOW_CODE, NOW_NAME, _, NOW_BLURB = TRAIN[NOW_INDEX]


def milestone_state(code: str) -> str:
    i = next(i for i, m in enumerate(TRAIN) if m[0] == code)
    return "done" if i < NOW_INDEX else ("now" if i == NOW_INDEX else "next")


def anchor(code: str) -> str:
    return code.lower()


# ---------------------------------------------------------------------------
# Docs navigation. An entry is (kind, target, label, milestone-or-None,
# muted): a planned capability without a page links to its roadmap stop, and
# a page about work not yet reachable from a client is muted.

PAGE, SOON, EXT = "page", "soon", "ext"

DOCS_NAV = [
    ("Get started", [
        (PAGE, "index", "Introduction", None, False),
        (PAGE, "quickstart", "Quickstart", None, False),
        (PAGE, "compat", "Redis compatibility", None, False),
        (PAGE, "roadmap", "Status & roadmap", None, False),
    ]),
    ("Concepts", [
        (PAGE, "cells", "Cells & ownership", None, False),
        (PAGE, "fabric", "The fabric", None, False),
        (PAGE, "log", "The log spine", None, False),
        (PAGE, "durability", "Namespaces & durability", None, False),
        (PAGE, "tiering", "Beyond RAM", "M4", False),
        (PAGE, "simulation", "Deterministic simulation", None, False),
    ]),
    ("Data models", [
        (PAGE, "strings", "Strings & keys", None, False),
        (PAGE, "json", "JSON documents", None, False),
        (PAGE, "indexes", "Indexes & PartiQL", "M4", True),
        (SOON, "roadmap.html#m5", "Hashes, lists, sets", "M5", True),
        (SOON, "roadmap.html#m7", "Streams & queues", "M7", True),
        (SOON, "roadmap.html#m8", "Vectors", "M8", True),
    ]),
    ("Operate", [
        (PAGE, "configuration", "Configuration", None, False),
        (PAGE, "memory", "Memory & eviction", None, False),
        (PAGE, "observability", "Observability", None, False),
        (SOON, "roadmap.html#m4", "Security", "M4", True),
    ]),
    ("Engineering", [
        (PAGE, "laws", "Design laws", None, False),
        (PAGE, "style", "InfinityStyle", None, False),
        (EXT, "../index.html#evidence", "Evidence ledger", None, False),
    ]),
]

DOC_PAGES = [e[1] for _, items in DOCS_NAV for e in items if e[0] == PAGE]


def doc_section(slug: str) -> str:
    for section, items in DOCS_NAV:
        if any(e[0] == PAGE and e[1] == slug for e in items):
            return section
    raise SystemExit(f"error: docs page {slug!r} is not in DOCS_NAV")


def doc_label(slug: str) -> str:
    for _, items in DOCS_NAV:
        for e in items:
            if e[0] == PAGE and e[1] == slug:
                return e[2]
    raise SystemExit(f"error: docs page {slug!r} is not in DOCS_NAV")


def nav_tag(milestone):
    if milestone is None:
        return ""
    return milestone + (" · now" if milestone == NOW else "")


# ---------------------------------------------------------------------------
# Blog. Order is the index order; a post without a page is being written.

POSTS = [
    dict(slug=None, title="Beyond RAM, per core", date="Now writing", topic="M4 · Engineering",
         group="Engineering", kind="tiers", seed=41,
         excerpt="One logical address space per cell and namespace: hot records stay in memory, "
                 "cold ones move to NVMe, and a command that needs one suspends instead of blocking the core."),
    dict(slug="every-failure-is-a-seed", kind="seed", seed=0xC0FFEE),
    dict(slug="why-this-site-has-no-benchmarks", kind="ledger", seed=19),
    dict(slug="trust-before-the-first-tag", kind="gates", seed=7),
    dict(slug="documents-as-tape", kind="tape", seed=3),
    dict(slug="durability-belongs-to-the-namespace", kind="log", seed=11),
    dict(slug="architecture-first-the-m0-verdict", kind="cells", seed=5),
]
FEATURED = dict(slug="why-vortex-failed", kind="spiral", seed=1)
BLOG_TOPICS = ["All", "Engineering", "Release notes", "Testing", "Evidence"]
AUTHOR = ("Kevin Caicedo", "KC", "https://github.com/kevincaicedo")

# ---------------------------------------------------------------------------
# Pixel art: every sprite is a grid of module characters turned into one
# SVG path per color, so it stays crisp at integer scales.


def pix(rows, keys, y0=0):
    d = []
    for y, row in enumerate(rows):
        x = 0
        while x < len(row):
            if row[x] in keys:
                n = 1
                while x + n < len(row) and row[x + n] in keys:
                    n += 1
                d.append(f"M{x} {y + y0}h{n}v1h-{n}z")
                x += n
            else:
                x += 1
    return "".join(d)


LOGO = ["..###.....###..", ".#...#...#...#.", "#.....#.#.....#", "#......s......#",
        "#.....#.#.....#", ".#...#...#...#.", "..###.....###.."]
LOGO_SMALL = [".###...###.", "#...#.#...#", "#....s....#", "#...#.#...#", ".###...###."]


def logo_svg(w=45, h=21, label=None):
    aria = f'role="img" aria-label="{label}"' if label else 'aria-hidden="true"'
    return (f'<svg class="px" width="{w}" height="{h}" viewBox="0 0 15 7" {aria}>'
            f'<path class="f-ink" d="{pix(LOGO, "#")}"/><path class="f-sig" d="{pix(LOGO, "s")}"/></svg>')


def favicon_svg():
    return ('<svg xmlns="http://www.w3.org/2000/svg" viewBox="-1 -4 13 13" shape-rendering="crispEdges">'
            '<style>.i{fill:#0E0E0C}.s{fill:#FF5A1F}@media (prefers-color-scheme:dark){.i{fill:#EDEDE7}.s{fill:#FF6526}}</style>'
            f'<path class="i" d="{pix(LOGO_SMALL, "#")}"/><path class="s" d="{pix(LOGO_SMALL, "s")}"/></svg>\n')


def moss_grid(pose="std", frame="a"):
    g = [["."] * 30 for _ in range(15)]
    ext = [(7, 19), (5, 21), (4, 22), (3, 23), (2, 24), (2, 25), (1, 26), (1, 26), (1, 26), (1, 26), (2, 25), (3, 24)]
    for y, (l, r) in enumerate(ext):
        for x in range(l, r + 1):
            g[y][x] = "#" if (y in (0, 11) or x in (l, r)) else "w"
    g[1][6] = "#"
    g[1][20] = "#"
    for y in (3, 5, 7, 9):
        g[y][9] = "g"
        g[y][15] = "g"
    g[8][24] = "#"
    if pose == "seed":
        g[8][28] = "a"
    else:
        g[7][20] = "a"
    for i, x in enumerate((5, 10, 15, 20)):
        lifted = frame == "b" and i in (0, 2)
        g[12][x] = g[12][x + 1] = "#"
        if lifted:
            g[13][x - 1] = g[13][x + 2] = "#"
        else:
            g[13][x] = g[13][x + 1] = "#"
            g[14][x - 1] = g[14][x + 2] = "#"
    for x in (8, 13, 18, 23):
        g[12][x] = "g"
    return ["".join(r) for r in g]


def moss_svg(w, h, cls="walk", label="Moss the tardigrade, walking", pose="std", style=""):
    a = moss_grid(pose, "a")
    b = moss_grid(pose, "b")
    body, legs_a, legs_b = a[:12], a[12:], b[12:]
    aria = f'role="img" aria-label="{label}"' if label else 'aria-hidden="true"'
    st = f' style="{style}"' if style else ""
    eye = ('<path class="f-ink eye-open" d="M21 5h1v2h-1z"/>'
           '<path class="f-ink eye-closed" d="M20 6h2v1h-2z"/>')
    legs = (f'<g class="legs-a"><path class="f-ink" d="{pix(legs_a, "#", 12)}"/>'
            f'<path class="f-dim" d="{pix(legs_a, "g", 12)}"/></g>')
    if cls == "walk":
        legs += (f'<g class="legs-b"><path class="f-ink" d="{pix(legs_b, "#", 12)}"/>'
                 f'<path class="f-dim" d="{pix(legs_b, "g", 12)}"/></g>')
    return (f'<svg class="px moss {cls}" width="{w}" height="{h}" viewBox="0 0 30 15" {aria}{st}>'
            f'<path class="f-raise" d="{pix(body, "w")}"/><path class="f-dim" d="{pix(body, "g")}"/>'
            f'<path class="f-ink" d="{pix(body, "#")}"/><path class="f-sig" d="{pix(body, "a")}"/>'
            f'{eye}{legs}</svg>')


# The design's seeded generator (a mulberry32 variant), ported bit-exactly:
# every operation is mod 2^32, which is what JavaScript's ToInt32/ToUint32
# reduce to.
def rng(seed):
    s = seed & 0xFFFFFFFF

    def imul(a, b):
        return (a * b) & 0xFFFFFFFF

    def nxt():
        nonlocal s
        s = (s + 0x6D2B79F5) & 0xFFFFFFFF
        t = s
        t = imul(t ^ (t >> 15), t | 1)
        t = (t ^ ((t + imul(t ^ (t >> 7), t | 61)) & 0xFFFFFFFF)) & 0xFFFFFFFF
        return ((t ^ (t >> 14)) & 0xFFFFFFFF) / 4294967296

    return nxt


BAYER = [0, 8, 2, 10, 12, 4, 14, 6, 3, 11, 1, 9, 15, 7, 13, 5]


def _num(v):
    r = repr(round(v, 6))
    return r[:-2] if r.endswith(".0") else r


def dots(g, key):
    d = []
    for y, row in enumerate(g):
        for x, c in enumerate(row):
            if c == key:
                d.append(f"M{_num(x + 0.12)} {_num(y + 0.12)}h.76v.76h-.76z")
    return "".join(d)


def cover(kind, W, H, seed):
    """A post's dithered cover, ported from the design's generator."""
    g = [["."] * W for _ in range(H)]

    def th(x, y):
        return (BAYER[(x & 3) + ((y & 3) << 2)] + 0.5) / 16

    def put(x, y, c):
        if 0 <= x < W and 0 <= y < H:
            g[y][x] = c

    rnd = rng(seed)

    def sm(u):
        u = min(1.0, max(0.0, u))
        return u * u * (3 - 2 * u)

    if kind == "spiral":
        cx, cy = W / 2, H / 2
        R = max(W, H) * 0.55
        for y in range(H):
            for x in range(W):
                dx, dy = x + 0.5 - cx, (y + 0.5 - cy) * 1.15
                r = math.sqrt(dx * dx + dy * dy)
                if r < 1.8:
                    continue
                a = math.atan2(dy, dx)
                arm = 0.5 + 0.5 * math.cos(2 * a - r * 0.42)
                v = arm * arm * (1 - sm((r - 3) / (R - 3))) * 1.15
                if v > th(x, y):
                    put(x, y, "g" if r > R * 0.62 else "#")
        put(math.floor(cx), math.floor(cy), "s")
    elif kind == "cells":
        pitch = (W - 4) // 4
        size = pitch - 3
        bh = min(size, math.floor(H * 0.36))
        ys = [math.floor(H * 0.12), math.floor(H * 0.56)]
        for j in range(2):
            for i in range(4):
                x0, y0 = 3 + i * pitch, ys[j]
                dens = 0.15 + rnd() * 0.45
                for y in range(bh):
                    for x in range(size):
                        edge = x == 0 or y == 0 or x == size - 1 or y == bh - 1
                        if edge:
                            put(x0 + x, y0 + y, "#")
                        elif dens > th(x0 + x, y0 + y):
                            put(x0 + x, y0 + y, "g")
                if j == 1 and i == 2:
                    for y in range(1, bh - 1):
                        for x in range(1, size - 1):
                            put(x0 + x, y0 + y, ".")
                    put(x0 + size // 2, y0 + bh // 2, "s")
    elif kind == "log":
        for k in range(4):
            y = math.floor(H * 0.2) + k * math.floor(H * 0.2)
            x = 3
            end = math.floor(W * 0.68) if k == 3 else W - 4
            while x < end:
                w = 2 + math.floor(rnd() * 6)
                i = 0
                while i < w and x + i < end:
                    put(x + i, y, "#" if k == 3 else "g")
                    put(x + i, y + 1, "#" if k == 3 else "g")
                    i += 1
                x += w + 1
            if k == 3:
                put(end + 1, y, "s")
                put(end + 1, y + 1, "s")
    elif kind == "tape":
        indent = [0, 1, 2, 2, 1, 2, 1, 0]
        for k in range(len(indent)):
            y = 2 + k * 3
            if y >= H - 1:
                break
            x = 4 + indent[k] * 4
            kl = 3 + math.floor(rnd() * 3)
            for i in range(kl):
                put(x + i, y, "#")
            x += kl + 2
            vl = 4 + math.floor(rnd() * 12)
            for i in range(vl):
                put(x + i, y, "g")
            if k == 3:
                put(x, y, "s")
    elif kind == "seed":
        rows = (H - 7) // 2
        pattern = [rnd() < 0.33 for _ in range(rows * (W - 6))]
        for p in range(2):
            y0 = 2 if p == 0 else 2 + rows + 3
            for y in range(rows):
                for x in range(W - 6):
                    if pattern[y * (W - 6) + x]:
                        put(3 + x, y0 + y, "#")
                    else:
                        put(3 + x, y0 + y, "g" if (x + y) % 3 == 0 else ".")
            put(3 + math.floor((W - 6) * 0.62), y0 + rows // 2, "s")
    elif kind == "ledger":
        for x in range(3, W - 3):
            put(x, 3, "g")
        for k in range(5):
            y = 7 + k * 4
            if y >= H - 2:
                break
            ln = 14 + math.floor(rnd() * 16)
            for x in range(ln):
                if 0.72 > th(3 + x, y):
                    put(3 + x, y, "#")
            bx = W - 8
            for i in range(3):
                put(bx + i, y - 1, "#")
                put(bx + i, y + 1, "#")
            put(bx, y, "#")
            put(bx + 2, y, "#")
    elif kind == "gates":
        mid = H // 2
        gx = [math.floor(W * (i + 1) / 6) for i in range(5)]
        for i in range(5):
            for y in range(3, H - 3):
                opened = i < 2 and abs(y - mid) <= 1
                if not opened:
                    put(gx[i], y, "#")
                    put(gx[i] + 1, y, "#")
        for x in range(2, gx[2] - 3):
            if (x & 1) == 0:
                put(x, mid, "g")
        put(gx[2] - 2, mid, "s")
    elif kind == "tiers":
        a, b = math.floor(W * 0.45), math.floor(W * 0.7)
        for y in range(5, H - 5):
            for x in range(2, W - 3):
                dens = 0.2 if x < a else (0.5 if x < b else 0.85)
                if dens > th(x, y):
                    put(x, y, "g" if x < a else "#")
        for y in range(3, H - 3, 2):
            put(a, y, "g")
            put(b, y, "g")
        for y in range(5, H - 5):
            put(W - 3, y, ".")
        put(W - 2, H // 2, "s")
        put(W - 2, H // 2 - 1, "s")
    else:
        raise SystemExit(f"error: unknown cover kind {kind!r}")
    return dict(ink=dots(g, "#"), muted=dots(g, "g"), sig=dots(g, "s"))


def cover_svg(kind, seed, W, H, w, h, label=None):
    c = cover(kind, W, H, seed)
    aria = f'role="img" aria-label="{label}"' if label else 'aria-hidden="true"'
    return (f'<svg class="px cover" width="{w}" height="{h}" viewBox="0 0 {W} {H}" {aria}>'
            f'<path class="f-dim" d="{c["muted"]}"/><path class="f-ink" d="{c["ink"]}"/>'
            f'<path class="f-sig" d="{c["sig"]}"/></svg>')


# ---------------------------------------------------------------------------
# Icons (inline stroke SVG, text-colored).

ICON_EXT = ('<svg width="10" height="10" viewBox="0 0 10 10" fill="none" aria-hidden="true">'
            '<path d="M2 8L8 2M3 2h5v5" stroke="currentColor" stroke-width="1.4"/></svg>')
ICON_COPY = ('<svg width="14" height="14" viewBox="0 0 14 14" fill="none" aria-hidden="true">'
             '<rect x="4.5" y="4.5" width="7" height="7" stroke="currentColor"/>'
             '<path d="M9.5 2.5h-7v7" stroke="currentColor"/></svg>')
ICON_SEARCH = ('<svg width="14" height="14" viewBox="0 0 14 14" fill="none" aria-hidden="true">'
               '<circle cx="6" cy="6" r="4.25" stroke="currentColor" stroke-width="1.4"/>'
               '<path d="M9.2 9.2L12.5 12.5" stroke="currentColor" stroke-width="1.4"/></svg>')
ICON_MENU = ('<svg width="20" height="12" viewBox="0 0 20 12" aria-hidden="true">'
             '<rect x="0" y="1" width="20" height="2" fill="currentColor"/>'
             '<rect x="0" y="9" width="20" height="2" fill="currentColor"/></svg>')
ICON_CLOSE = ('<svg width="16" height="16" viewBox="0 0 16 16" fill="none" aria-hidden="true">'
              '<path d="M2 2L14 14M14 2L2 14" stroke="currentColor" stroke-width="2"/></svg>')
ICON_THEME = ('<svg width="14" height="14" viewBox="0 0 14 14" fill="none" aria-hidden="true">'
              '<circle cx="7" cy="7" r="5.25" stroke="currentColor" stroke-width="1.4"/>'
              '<path d="M7 1.75a5.25 5.25 0 0 1 0 10.5z" fill="currentColor"/></svg>')
ICON_CHEVRON = ('<svg width="12" height="12" viewBox="0 0 12 12" fill="none" aria-hidden="true">'
                '<path d="M2.5 4.5L6 8l3.5-3.5" stroke="currentColor" stroke-width="1.5"/></svg>')
ICON_CHECK = ('<svg width="14" height="14" viewBox="0 0 14 14" fill="none" aria-hidden="true">'
              '<path d="M2.5 7.5l3 3 6-7" stroke="currentColor" stroke-width="1.5"/></svg>')

# ---------------------------------------------------------------------------
# Figures. Each keeps the design's geometry on a fixed stage and scales as a
# unit with its container (site.css: .fig-stage).


def key_dots():
    kd = ["#..#..#" if y % 3 == 0 else "......." for y in range(13)]
    return pix(kd, "#")


def fig_ownership():
    kd = key_dots()
    cells = "".join(
        f'<div class="cell b2"><svg class="px" width="28" height="52" viewBox="0 0 7 13" aria-hidden="true">'
        f'<path class="f-faint" d="{kd}"/></svg><div class="cell-l fs">c{i}</div></div>' for i in range(8))
    flashes = "".join(
        f'<div class="flash b2" style="left:{x}px;animation-delay:{d}s"></div>'
        for x, d in ((152, 0), (152, 2), (416, 4)))
    drops = "".join(
        f'<div class="drop {k}" style="animation-delay:{d}s"><span class="sq10 bg-sig"></span>'
        f'<span class="fs ink">{html.escape(t)}</span></div>'
        for t, k, d in (("user:42", "drop-a", 0), ("{user:42}.cart", "drop-a", 2), ("sku:9", "drop-c", 4)))
    return f'''<figure class="fig" aria-label="Animated diagram: three requests are hashed to their owning cell among eight cells"><div class="fig-stage">
<div class="fl" style="left:19px;top:15px">fig. 01<span class="fl-sec"> — ownership</span></div>
<div class="fl fl-sec" style="right:19px;top:15px">8 cells · 8 cores · no locks</div>
<div class="fs fl-sec" style="left:19px;top:67px">request</div>
<div class="fs fl-sec" style="right:19px;top:133px">crc16(key) mod 16384 → slot → cell</div>
<div class="fs fl-alt" style="right:19px;top:118px">crc16(key) mod 16384</div>
<div class="dash" style="left:19px;right:19px;top:155px"></div>
<div class="cells" style="left:20px;top:215px">{cells}</div>
{flashes}{drops}
<div class="fs fl-sec" style="left:19px;bottom:15px">{{user:42}}.cart hashes on user:42, so it lands on the same cell</div>
</div></figure>'''


def fig_fabric():
    slots = lambda n: "".join('<span class="slot"></span>' for _ in range(n))  # noqa: E731
    msgs = "".join(
        f'<div class="msg" style="left:{39 + 16 * i}px;animation-name:inf-f-move,inf-f-in{i}"></div>' for i in range(4))
    return f'''<figure class="fig" aria-label="Animated diagram: four messages collect in one cell, travel together through a ring to another cell, and a credit returns"><div class="fig-stage">
<div class="fl" style="left:19px;top:15px">fig. 02<span class="fl-sec"> — the fabric</span></div>
<div class="fl fl-sec" style="right:19px;top:15px">SPSC rings · credits</div>
<div class="box" style="left:19px;top:95px;width:150px;height:232px"></div>
<div class="fs ink" style="left:31px;top:107px">cell 2<span class="fl-sec"> · core 2</span></div>
<div class="fl fl10" style="left:31px;top:181px">outbox</div>
<div class="fs fs10 fl-sec" style="left:31px;top:295px">one batch per loop</div>
<div class="slots" style="left:38px;top:205px">{slots(4)}</div>
<div class="box" style="left:389px;top:95px;width:150px;height:232px"></div>
<div class="fs ink" style="left:401px;top:107px">cell 6<span class="fl-sec"> · core 6</span></div>
<div class="fl fl10" style="left:401px;top:181px">inbox</div>
<div class="slots" style="left:408px;top:205px">{slots(4)}</div>
<div class="bell" style="left:401px;top:253px"></div>
<div class="bell bell-on" style="left:401px;top:253px"></div>
<div class="fs fs10" style="left:419px;top:251px">doorbell</div>
<div class="fl fl10" style="left:215px;top:179px">ring 2 → 6</div>
<div class="rule" style="left:169px;width:220px;top:198px"></div>
<div class="rule" style="left:169px;width:220px;top:226px"></div>
<div class="slots" style="left:215px;top:205px">{slots(8)}</div>
{msgs}
<div class="dash" style="left:94px;width:370px;top:355px"></div>
<div class="fl fl10 fl-sec" style="left:0;right:0;top:363px;text-align:center">credits flow back</div>
<div class="credit" style="left:449px;top:351px"></div>
<div class="fs fl-sec" style="left:19px;bottom:15px">4 messages · 1 batch · 1 doorbell</div>
</div></figure>'''


def fig_log():
    seg = lambda n, c: "".join(f'<span class="rec {c}"></span>' for _ in range(n))  # noqa: E731
    rows = [("cache", "the log, switched off (memory mode)", "log off"),
            ("durable KV", "the log, plus an index and checkpoints", "log + index"),
            ("queue", "the log, read forward by consumer groups", "log read forward"),
            ("replica", "the log, shipped to another node", "log shipped"),
            ("CDC", "the log, handed to a subscriber", "log to a subscriber")]
    table = "".join(f'<div class="lrow"><span class="fs ink lkey">{k}</span><span class="fl-sec">{v}</span>'
                    f'<span class="fl-alt">{short}</span></div>' for k, v, short in rows)
    return f'''<figure class="fig" aria-label="Animated diagram: records append to the active log segment while a queue reader and a replica follow behind the tail"><div class="fig-stage">
<div class="fl" style="left:19px;top:15px">fig. 03<span class="fl-sec"> — the log spine</span></div>
<div class="fl fl-sec" style="right:19px;top:15px">cell 4 · seg-000017.ilog</div>
<div class="recs" style="left:19px;top:75px">{seg(10, "rec-dim")}</div>
<div class="recs" style="left:151px;top:75px">{seg(10, "rec-dim")}</div>
<div class="recs" style="left:283px;top:75px">{seg(20, "rec-ink")}</div>
<div class="vrule" style="left:147px;top:69px"></div>
<div class="vrule" style="left:279px;top:69px"></div>
<div class="tailmask" style="top:71px;right:17px"><div class="tailbar"></div><div class="fl fl10 sig-text" style="left:4px;top:-20px">tail</div></div>
<div class="fl fl10" style="left:19px;top:109px">seg 15<span class="fl-sec"> · sealed</span></div>
<div class="fl fl10" style="left:151px;top:109px">seg 16<span class="fl-sec"> · sealed</span></div>
<div class="fl fl10" style="left:283px;top:109px">seg 17<span class="fl-sec"> · active</span></div>
<div class="reader reader-q" style="left:199px;top:133px"><span class="sq8 bg-ink"></span><span class="fs fs10 ink2">queue</span></div>
<div class="reader reader-r" style="left:249px;top:133px"><span class="sq8 sq-line"></span><span class="fs fs10 ink2">replica</span></div>
<div class="rule" style="left:19px;right:19px;top:183px"></div>
<div class="ltable" style="left:19px;right:19px;top:195px">{table}</div>
<div class="fs fl-sec" style="left:19px;bottom:15px">one mechanism, made correct once</div>
</div></figure>'''


def dst_events():
    r = rng(0xC0FFEE)
    out = []
    for i in range(192):
        on = r() < 0.36
        out.append("ev ev-f" if i == 117 else ("ev ev-on" if on else "ev"))
    return "".join(f'<span class="{c}"></span>' for c in out)


def fig_dst():
    ev = dst_events()
    run = (f'<div class="evgrid">{ev}</div><div class="dmask"><div class="dbar"></div></div>')
    return f'''<figure class="fig" aria-label="Animated diagram: a simulated run and its replay with the same seed produce identical events, including the same injected power cut"><div class="fig-stage">
<div class="fl" style="left:19px;top:15px">fig. 04<span class="fl-sec"> — deterministic simulation</span></div>
<div class="fl fl-sec" style="right:19px;top:15px">inf-sim</div>
<div class="fs fs13 ink" style="left:19px;top:51px"><span class="muted">$</span> inf-sim --seed 0xC0FFEE</div>
<div class="fl fl10 up6" style="left:19px;top:93px">run</div>
<div class="evrun" style="left:19px;top:111px">{run}</div>
<div class="fl fl10 up6" style="left:19px;top:205px">replay · same seed</div>
<div class="evrun" style="left:19px;top:223px">{run}</div>
<div class="fs fs12 ink dst-done" style="left:19px;top:325px">{ICON_CHECK}identical: same events, same state<span class="fl-sec">, byte for byte</span></div>
<div class="fs legend fl-sec" style="left:19px;bottom:15px"><span><span class="sq8 bg-ink"></span>event</span><span><span class="sq8 bg-sig"></span>fault injected: power cut</span><span><span class="sq8 ev-idle"></span>idle</span></div>
</div></figure>'''


def fig_redis():
    lines = [("$", "redis-cli -p 6379"), ("p", "PING"), ("o", "PONG"), ("p", 'SET user:42 "ada"'), ("o", "OK"),
             ("p", "INF.NS CREATE ledger MODE durable FSYNC always"), ("o", "OK"),
             ("p", "JSON.SET doc:{u42} $ '{\"plan\":\"pro\"}'"), ("o", "OK"),
             ("p", "JSON.GET doc:{u42} $.plan"), ("o", '"[\\"pro\\"]"')]
    out = []
    for kind, text in lines:
        t = html.escape(text, quote=False)
        if kind == "$":
            out.append(f'<div><span class="muted">$</span> {t}</div>')
        elif kind == "p":
            out.append(f'<div><span class="muted"><span class="fl-sec">127.0.0.1:6379</span>&gt;</span> {t}</div>')
        else:
            out.append(f'<div class="muted">{t}</div>')
    return f'''<figure class="fig" aria-label="Animated terminal: a redis-cli session connects, sets a key, makes a durable namespace and stores a JSON document"><div class="fig-stage">
<div class="fl" style="left:19px;top:15px">fig. 05<span class="fl-sec"> — redis-cli</span></div>
<div class="fl fl-sec" style="right:19px;top:15px">RESP2 · RESP3 · port 6379</div>
<div class="term" style="left:19px;top:59px">{"".join(out)}</div>
<div class="termmask"><div class="caret"></div></div>
<div class="rule" style="left:19px;right:19px;top:343px"></div>
<div class="fl fl10 statuses" style="left:19px;top:357px"><span class="st st-on">full</span><span class="st">partial</span><span class="st">stub</span><span class="st st-dash">absent</span></div>
<div class="fs fl-sec" style="left:19px;bottom:15px">every command, marked in a generated matrix</div>
</div></figure>'''


def fig_tiers():
    r2 = rng(0x1D0C2026)
    sizes = [6, 10, 10, 14, 14, 18, 26, 40]
    recs, used = [], 0
    while 1320 - used > 48:
        w = sizes[math.floor(r2() * len(sizes))]
        recs.append(w)
        used += w + 4
    recs.append(1320 - used - 4)
    track = "".join(f'<span style="width:{w}px"></span>' for w in recs + recs)
    return f'''<figure class="fig fig-wide" aria-label="Animated diagram: records drift from the mutable in-memory tail toward the on-disk cold region; an update copies a record to the tail"><div class="fig-stage">
<div class="track" style="left:-1px;top:95px">{track}</div>
<div class="veil" style="left:-1px;top:87px;width:520px;--veil:70%"></div>
<div class="veil" style="left:519px;top:87px;width:360px;--veil:28%"></div>
<div class="vdash" style="left:519px;top:11px"></div>
<div class="vdash" style="left:879px;top:11px"></div>
<div class="zone" style="left:15px;top:15px"><div class="fl">On disk · cold</div><div class="zone-d">read via io_uring; the command suspends, the core does not</div></div>
<div class="zone" style="left:535px;top:15px"><div class="fl">Read-only · RAM</div><div class="zone-d">an update copies the record to the tail</div></div>
<div class="zone" style="left:895px;top:15px"><div class="fl">Mutable · RAM</div><div class="zone-d">updated in place</div></div>
<div class="tailpin" style="left:1189px;top:83px"></div>
<div class="hot" style="left:699px;top:95px"></div>
<div class="fl fl10" style="left:527px;top:133px">head</div>
<div class="fl fl10" style="left:887px;top:133px">read-only boundary</div>
<div class="fl fl10 sig-text" style="right:15px;top:133px">tail</div>
<div class="fs" style="left:15px;bottom:13px">one logical address space per cell · index slots hold 48-bit addresses · compaction slices advance the head</div>
</div></figure>'''


FIGURES = {
    "ownership": fig_ownership, "fabric": fig_fabric, "log": fig_log,
    "dst": fig_dst, "redis": fig_redis, "tiers": fig_tiers,
}

# ---------------------------------------------------------------------------
# Train renderings.


def train_grid():
    out = []
    for i, (code, name, tag, _) in enumerate(TRAIN):
        state = milestone_state(code)
        here = '<div class="rm-here mono">you are here</div>' if state == "now" else ""
        line = "rm-line rm-line-done" if state == "done" else "rm-line"
        code_cls = "rm-code rm-code-on" if state != "next" else "rm-code"
        out.append(
            f'<li class="rm-stop rm-{state}">{here}<div class="rm-track"><div class="{line}"></div>'
            f'<div class="rm-mark"></div></div><div class="mono {code_cls}">{code}</div>'
            f'<div class="rm-name">{html.escape(name)}</div><div class="mono rm-tag">{html.escape(tag)}</div></li>')
    return "".join(out)


TRAIN_ROW = [("Beyond RAM", "M4"), ("Indexes & PartiQL subset", "M4"), ("Transactions", "M6"),
             ("Replication & HA", "M9"), ("WASM reducers & embedded mode", "M10")]


def train_row():
    out = []
    for name, code in TRAIN_ROW:
        dot = '<span class="sq6 bg-sig" aria-hidden="true"></span>' if code == NOW else ""
        out.append(f"<span>{dot}{html.escape(name)} · {code}</span>")
    return "".join(out)


def train_list(link_prefix=None):
    out = []
    for code, name, tag, _ in TRAIN:
        state = milestone_state(code)
        tag_txt = "now" if state == "now" else TRAIN_SHORT_TAG.get(tag, tag)
        label = (f'<a href="{link_prefix}#{anchor(code)}">{html.escape(name)}</a>' if link_prefix is not None
                 else html.escape(name))
        out.append(
            f'<li class="vr vr-{state}"><div class="vr-track"><div class="vr-line"></div><div class="vr-mark"></div></div>'
            f'<span class="mono vr-code">{code}</span><span class="vr-name">{label}</span>'
            f'<span class="mono vr-tag">{html.escape(tag_txt)}</span></li>')
    return "".join(out)


# ---------------------------------------------------------------------------
# Page chrome.


def head(title, description, rel, path, extra=""):
    canonical = SITE_URL + path
    return f'''<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{html.escape(title)}</title>
<meta name="description" content="{html.escape(description)}">
<link rel="canonical" href="{canonical}">
<meta property="og:type" content="website">
<meta property="og:site_name" content="InfinityDB">
<meta property="og:title" content="{html.escape(title)}">
<meta property="og:description" content="{html.escape(description)}">
<meta property="og:url" content="{canonical}">
<meta name="theme-color" content="#F4F4F0" media="(prefers-color-scheme: light)">
<meta name="theme-color" content="#0B0B0A" media="(prefers-color-scheme: dark)">
<link rel="icon" href="{rel}assets/favicon.svg" type="image/svg+xml">
<link rel="alternate" type="application/rss+xml" title="InfinityDB engineering notes" href="{rel}blog/feed.xml">
<link rel="preconnect" href="https://fonts.googleapis.com">
<link rel="preconnect" href="https://fonts.gstatic.com" crossorigin>
<link rel="stylesheet" href="{FONTS}">
<link rel="stylesheet" href="{rel}assets/site.css">
<script>try{{var t=localStorage.getItem("inf-theme");if(t==="light"||t==="dark")document.documentElement.setAttribute("data-theme",t)}}catch(e){{}}</script>
<script src="{rel}assets/site.js" defer></script>{extra}
</head>
'''


def brand(rel, size="lg", suffix=""):
    name_cls = "brand-name" + (" brand-name-sm" if size == "sm" else "")
    return (f'<a class="brand" href="{rel}index.html" aria-label="InfinityDB home">{logo_svg()}'
            f'<span class="{name_cls}">InfinityDB</span>{suffix}</a>')


def announce(rel):
    return f'''<div class="announce mono">
<span class="sq8 bg-sig" aria-hidden="true"></span>
<span>{NOW_CODE} · {html.escape(NOW_NAME)}<span class="m-only"> — in progress</span></span>
<span class="muted d-only">{html.escape(NOW_BLURB)} — in progress</span>
<a class="d-only u" href="{rel}index.html#roadmap">Follow the build</a>
</div>'''


TOP_LINKS = [("Docs", "docs/index.html", "docs"), ("Architecture", "index.html#how", None),
             ("Roadmap", "index.html#roadmap", None), ("Evidence", "index.html#evidence", None),
             ("Blog", "blog/index.html", "blog")]


def site_nav(rel, active=None):
    links = []
    for label, href, key in TOP_LINKS:
        if key and key == active:
            links.append(f'<a href="{rel}{href}" aria-current="page" class="on"><span class="sq6 bg-sig" aria-hidden="true"></span>{label}</a>')
        else:
            links.append(f'<a href="{rel}{href}">{label}</a>')
    return f'''<header class="nav">
<div class="wrap nav-in">
{brand(rel)}
<nav class="nav-links mono" aria-label="Primary">{"".join(links)}</nav>
<div class="nav-cta">
<a class="btn-sm btn-line mono" href="{REPO}">GitHub{ICON_EXT}</a>
<a class="btn-sm btn-ink" href="{rel}index.html#start">Get started</a>
</div>
<button type="button" class="icon-btn nav-burger" data-menu-open aria-controls="menu" aria-expanded="false" aria-label="Open menu">{ICON_MENU}</button>
</div>
</header>
{site_menu(rel)}'''


def theme_switch():
    return ('<div class="theme-seg" role="group" aria-label="Theme">'
            '<button type="button" class="mono" data-theme-set="light" aria-pressed="false">Light</button>'
            '<button type="button" class="mono" data-theme-set="dark" aria-pressed="false">Dark</button></div>')


def site_menu(rel):
    items = "".join(
        f'<a href="{rel}{href}">{label}<span aria-hidden="true">→</span></a>' for label, href, _ in TOP_LINKS)
    return f'''<div class="menu" id="menu" hidden>
<div class="menu-head">{brand(rel, "sm")}<button type="button" class="icon-btn" data-menu-close aria-label="Close menu">{ICON_CLOSE}</button></div>
<nav class="menu-links" aria-label="Mobile">{items}<a class="menu-gh mono" href="{REPO}">GitHub{ICON_EXT}</a></nav>
<div class="menu-foot">
{theme_switch()}
<a class="btn btn-ink btn-block" href="{rel}index.html#start" data-menu-close>Get started</a>
<div class="menu-alpha mono"><span class="sq6 bg-sig" aria-hidden="true"></span>Alpha · {NOW_CODE} {html.escape(NOW_NAME)}</div>
</div>
</div>'''


def footer(rel):
    return f'''<footer class="footer">
<div class="wrap">
<div class="foot-grid">
<div class="foot-brand">
<div class="brand">{logo_svg()}<span class="brand-name">InfinityDB</span></div>
<p>A Redis-compatible, multi-model database. Deterministic to the core.</p>
</div>
<nav aria-label="Product" class="foot-col"><div class="eyebrow">Product</div><a href="{rel}index.html#how">How it works</a><a href="{rel}index.html#roadmap">Roadmap</a><a href="{rel}index.html#evidence">Evidence ledger</a><a href="{rel}blog/index.html">Blog</a></nav>
<nav aria-label="Docs" class="foot-col"><div class="eyebrow">Docs</div><a href="{rel}docs/quickstart.html">Quickstart</a><a href="{rel}docs/cells.html">Concepts</a><a href="{rel}docs/compat.html">Compatibility matrix</a><a href="{rel}docs/style.html">InfinityStyle</a></nav>
<nav aria-label="Project" class="foot-col"><div class="eyebrow">Project</div><a href="{REPO}">GitHub</a><a href="{REPO}/releases">Releases</a><a href="{REPO}/security">Security</a><a href="{REPO}/blob/main/LICENSE">License</a></nav>
</div>
<div class="foot-bar mono"><span>© 2026 InfinityDB contributors · Apache 2.0</span><span>Public numbers appear only when the ledger says Allowed.</span></div>
</div>
</footer>'''


def page(title, description, path, body, active=None, extra_head="", rel=None):
    if rel is None:
        rel = "../" * path.count("/")
    return (head(title, description, rel, path, extra_head)
            + '<body>\n<a class="skip mono" href="#main">Skip to content</a>\n'
            + announce(rel) + "\n" + site_nav(rel, active) + "\n"
            + body + "\n" + footer(rel) + "\n</body>\n</html>\n")


# ---------------------------------------------------------------------------
# Fragments: front-matter, code blocks, headings.

FM = re.compile(r"\A<!--\n(.*?)\n-->\n", re.S)


def front(text, path):
    m = FM.match(text)
    if not m:
        raise SystemExit(f"error: {path}: missing front-matter comment")
    meta = {}
    for line in m.group(1).splitlines():
        if not line.strip():
            continue
        k, sep, v = line.partition(":")
        if not sep:
            raise SystemExit(f"error: {path}: bad front-matter line {line!r}")
        meta[k.strip()] = v.strip()
    return meta, text[m.end():]


PROMPTS = ("$ ", "127.0.0.1:6379&gt; ", "&gt; ")


def code_block(label, body):
    lines = body.strip("\n").split("\n")
    has_prompt = any(l.startswith(PROMPTS) for l in lines)
    out, cont = [], False
    for l in lines:
        if not has_prompt:
            out.append(f'<span class="ln ln-plain">{l}</span>')
            continue
        p = next((p for p in PROMPTS if l.startswith(p)), None)
        if p:
            pr = p.rstrip()
            out.append(f'<span class="ln ln-cmd"><span class="pr">{pr}</span> {l[len(p):]}</span>')
            cont = l.rstrip().endswith("\\")
        elif cont:
            out.append(f'<span class="ln ln-cmd">{l}</span>')
            cont = l.rstrip().endswith("\\")
        else:
            out.append(f'<span class="ln ln-out">{l}</span>')
    code = "\n".join(out)
    if not label:
        return f'<div class="codeblock codeblock-bare"><pre class="mono">\n<code>{code}</code></pre></div>'
    copy_label = "Copy commands" if has_prompt else "Copy"
    return (f'<div class="codeblock"><div class="codeblock-head mono"><span>{html.escape(label)}'
            f'<span class="m-only"> · scroll →</span></span>'
            f'<button type="button" class="copy" data-copy aria-label="{copy_label}">{ICON_COPY}</button></div>'
            # The newline after <pre> is dropped by the parser; it keeps each
            # code line on its own source line: boot_refusal.rs reads the quoted refusal that way.
            f'<pre class="mono">\n<code>{code}</code></pre></div>')


PRE = re.compile(r'<pre class="code" data-label="([^"]*)">(.*?)</pre>', re.S)
H2 = re.compile(r'<h2 id="([a-z0-9-]+)">(.*?)</h2>', re.S)
NUM = re.compile(r'<span class="num">([^<]*)</span>')
TOKEN = re.compile(r"\{\{([a-z_:0-9-]+)\}\}")
TAGS = re.compile(r"<[^>]+>")


def render_fragment(body, h2_class):
    body = PRE.sub(lambda m: code_block(m.group(1), m.group(2)), body)
    toc = []

    def h2(m):
        hid, inner = m.group(1), m.group(2)
        text = TAGS.sub("", NUM.sub("", inner)).strip()
        toc.append((hid, text))
        inner = NUM.sub(r'<span class="pix num">\1</span>', inner)
        return f'<h2 id="{hid}" class="{h2_class}">{inner}</h2>'

    body = H2.sub(h2, body)
    body = re.sub(r'<table class="doc-table">(.*?)</table>',
                  r'<div class="table-wrap"><table class="doc-table">\1</table></div>', body, flags=re.S)
    return body, toc


def tokens(text, values, path):
    def sub(m):
        k = m.group(1)
        if k.startswith("fig:"):
            name = k[4:]
            if name not in FIGURES:
                raise SystemExit(f"error: {path}: unknown figure {name!r}")
            return FIGURES[name]()
        if k not in values:
            raise SystemExit(f"error: {path}: unknown token {{{{{k}}}}}")
        return values[k]

    return TOKEN.sub(sub, text)


# ---------------------------------------------------------------------------
# Landing.


def build_landing():
    src = (SRC / "index.html").read_text(encoding="utf-8")
    values = {
        "now_code": NOW_CODE, "now_name": html.escape(NOW_NAME), "repo": REPO,
        "train_grid": train_grid(), "train_list": train_list(), "train_row": train_row(),
        "train_stops": str(len(TRAIN)),
        "moss_start": moss_svg(300, 150, style=""),
        "check": ICON_CHECK, "copy": ICON_COPY, "ext": ICON_EXT,
    }
    body = tokens(src, values, "src/index.html")
    body, _ = render_fragment(body, "h2")
    desc = ("InfinityDB is a Redis-compatible, multi-model database: cache, durable key-value and JSON "
            "documents, built thread-per-core on a single log and tested by deterministic simulation.")
    return page("InfinityDB — Deterministic to the core", desc, "index.html", body)


# ---------------------------------------------------------------------------
# Docs.


def docs_header(rel):
    return f'''<header class="dh">
<div class="dh-left">
{brand(rel, "sm", '<span class="dh-slash mono">/ docs</span>')}
<div class="search" data-search>
<label for="doc-search" class="vh">Search the docs</label>
<input id="doc-search" type="search" placeholder="Search the docs" autocomplete="off" role="combobox" aria-expanded="false" aria-controls="doc-search-results" aria-autocomplete="list">
<span class="search-ico" aria-hidden="true">{ICON_SEARCH}</span>
<kbd class="mono" aria-hidden="true">⌘K</kbd>
<ul class="search-results" id="doc-search-results" role="listbox" hidden></ul>
</div>
</div>
<div class="dh-right">
<span class="badge mono"><span class="sq6 bg-ink" aria-hidden="true"></span>Alpha · {NOW_CODE}</span>
<a class="btn-xs btn-line mono" href="{REPO}">GitHub{ICON_EXT}</a>
<button type="button" class="icon-btn-sq" data-theme-toggle aria-label="Switch between light and dark theme">{ICON_THEME}</button>
</div>
<div class="dh-mobile">
<button type="button" class="icon-btn" data-docnav-open data-focus-search aria-label="Search the docs">{ICON_SEARCH}</button>
<button type="button" class="icon-btn" data-docnav-open aria-controls="docnav" aria-expanded="false" aria-label="Open menu">{ICON_MENU}</button>
</div>
</header>'''


def docs_sidebar(current):
    groups = []
    for section, items in DOCS_NAV:
        links = []
        for kind, target, label, ms, muted in items:
            # "now" marks a page about the current milestone; a planned
            # capability carries only its milestone.
            tag = nav_tag(ms) if kind == PAGE else (ms or "")
            tag_html = f'<span class="tag">{tag}</span>' if tag else ""
            if kind == PAGE:
                on = target == current
                cls = "dn" + (" dn-on" if on else "") + (" dn-soon" if muted else "")
                cur = ' aria-current="page"' if on else ""
                links.append(f'<a href="{target}.html" class="{cls}"{cur}>{html.escape(label)}{tag_html}</a>')
            elif kind == SOON:
                links.append(f'<a href="{target}" class="dn dn-soon">{html.escape(label)}{tag_html}</a>')
            else:
                links.append(f'<a href="{target}" class="dn">{html.escape(label)}</a>')
        groups.append(f'<nav aria-label="{html.escape(section)}" class="dn-group">'
                      f'<div class="eyebrow">{html.escape(section)}</div>{"".join(links)}</nav>')
    return "".join(groups)


def docs_page(slug, meta, body, toc):
    rel = "../"
    section = doc_section(slug)
    title = meta["title"]
    i = DOC_PAGES.index(slug)
    prev_slug = DOC_PAGES[i - 1] if i > 0 else None
    next_slug = DOC_PAGES[i + 1] if i + 1 < len(DOC_PAGES) else None
    pn = []
    if prev_slug:
        pn.append(f'<a class="pn" href="{prev_slug}.html"><span class="mono pn-k">← Previous</span>'
                  f'<span class="pn-t">{html.escape(doc_label(prev_slug))}</span></a>')
    else:
        pn.append('<span></span>')
    if next_slug:
        pn.append(f'<a class="pn pn-next" href="{next_slug}.html"><span class="mono pn-k">Next →</span>'
                  f'<span class="pn-t">{html.escape(doc_label(next_slug))}</span></a>')
    chips = ""
    if meta.get("chips"):
        parts = [c.strip() for c in meta["chips"].split("|") if c.strip()]
        cs = []
        for j, c in enumerate(parts):
            if j == 0:
                dot = ("bg-ink" if c.startswith("Available") else
                       "bg-sig" if c.startswith("In progress") else "sq-line")
                cs.append(f'<span class="chip-s"><span class="sq6 {dot}" aria-hidden="true"></span>{html.escape(c)}</span>')
            else:
                cs.append(f'<span class="chip-s">{html.escape(c)}</span>')
        chips = f'<div class="chips-s mono">{"".join(cs)}</div>'
    toc_html = "".join(
        f'<a href="#{hid}" data-toc="{hid}"{" class=on" if k == 0 else ""}><span class="sq5" aria-hidden="true"></span>{html.escape(t)}</a>'
        for k, (hid, t) in enumerate(toc))
    lede = f'<p class="doc-lede">{meta["lede"]}</p>' if meta.get("lede") else ""
    edit = f"{REPO}/blob/main/website/src/docs/{slug}.html" if slug != "compat" else f"{REPO}/blob/main/docs/compat-matrix.md"
    content = f'''<div class="dwrap">
<aside class="dside" id="docnav" aria-label="Documentation">
<div class="dside-head">{brand(rel, "sm", '<span class="dh-slash mono">/ docs</span>')}<button type="button" class="icon-btn" data-docnav-close aria-label="Close navigation">{ICON_CLOSE}</button></div>
<div class="dside-search"><label for="doc-search-m" class="vh">Search the docs</label><input id="doc-search-m" type="search" placeholder="Search the docs" autocomplete="off" data-search-input aria-controls="doc-search-results-m"><span class="search-ico" aria-hidden="true">{ICON_SEARCH}</span><ul class="search-results" id="doc-search-results-m" role="listbox" hidden></ul></div>
<div class="dside-nav">{docs_sidebar(slug)}</div>
<div class="dside-foot">{theme_switch()}</div>
</aside>
<main id="main" class="dmain">
<button type="button" class="dcrumb-m mono" data-docnav-open aria-label="Browse docs sections"><span><span class="muted">{html.escape(section)} /</span> {html.escape(doc_label(slug))}</span>{ICON_CHEVRON}</button>
<article class="doc{' doc-wide' if meta.get('layout') == 'wide' else ''}">
<div class="crumb mono"><span>{html.escape(section)}</span><span>/</span><span class="ink">{html.escape(doc_label(slug))}</span></div>
<h1 class="doc-h1">{html.escape(title)}</h1>
{lede}{chips}
{body}
<div class="pns">{"".join(pn)}</div>
</article>
<div class="dfoot mono"><span>© 2026 InfinityDB contributors · Apache 2.0</span><a href="{rel}index.html">infinitydb home</a></div>
</main>
<aside class="dtoc" aria-label="On this page">
<div class="eyebrow">On this page</div>
<nav class="toc">{toc_html}</nav>
<div class="dtoc-links mono"><a href="{edit}">Edit this page ↗</a><a href="{REPO}/issues/new">Report an issue ↗</a></div>
</aside>
</div>'''
    desc = meta.get("description") or TAGS.sub("", meta.get("lede", ""))
    path = f"docs/{slug}.html"
    return (head(f"{title} — InfinityDB Docs", desc, rel, path,
                 f'\n<script src="{rel}assets/search-index.js" defer></script>')
            + f'<body class="docs{" docs-wide" if meta.get("layout") == "wide" else ""}">\n<a class="skip mono" href="#main">Skip to content</a>\n'
            + docs_header(rel) + "\n" + content + '\n<div class="scrim" data-docnav-close hidden></div>\n</body>\n</html>\n')


def docs_values():
    return {
        "now_code": NOW_CODE, "now_name": html.escape(NOW_NAME), "repo": REPO,
        "train_list": train_list(link_prefix=""),
    }


def build_docs(out):
    index = []
    for slug in DOC_PAGES:
        path = SRC / "docs" / f"{slug}.html"
        if not path.exists():
            raise SystemExit(f"error: docs page {slug!r} has no source at {path}")
        values = docs_values()
        if slug == "compat":
            values.update(compat_values())
        meta, body = front(path.read_text(encoding="utf-8"), path)
        meta = {k: tokens(v, values, str(path)) for k, v in meta.items()}
        body = tokens(body, values, str(path))
        body, toc = render_fragment(body, "doc-h2")
        out[f"docs/{slug}.html"] = docs_page(slug, meta, body, toc)
        index.append(dict(t=doc_label(slug), s=doc_section(slug), u=f"{slug}.html", h=toc))
    extra = sorted(p.stem for p in (SRC / "docs").glob("*.html")) if (SRC / "docs").exists() else []
    orphans = [s for s in extra if s not in DOC_PAGES]
    if orphans:
        raise SystemExit(f"error: docs sources not in DOCS_NAV: {orphans}")
    out["assets/search-index.js"] = ("window.INF_SEARCH=" + json.dumps(index, ensure_ascii=False, separators=(",", ":")) + ";\n")


# ---------------------------------------------------------------------------
# Compatibility page, from the generated matrix artifact.

STATUS_ORDER = ["full", "partial", "stub", "extension", "internal"]
COLUMNS = ["command", "status", "since", "flags", "arity", "cases", "evidence", "notes"]

# A citation of a record the repository does not publish (a decision, an
# amendment's item, a review finding or date, a section of an unpublished
# document). The page states behavior and names only what a reader can open.
_CITE = (
    r"(?:ADR-\d{4}(?:\s+(?:[DA]\d+[a-z]?(?:\.\d+)?"
    r"(?:\s*[+/,]\s*[DA]\d+[a-z]?(?:\.\d+)?)*"
    r"|(?:first|second|third|fourth|fifth|sixth) amendment))?"
    r"|F-L\d{2}-\d{2}|FCR-[A-Z0-9]+-\d+|review \d{4}-\d{2}-\d{2}"
    r"|§\s?\d+(?:\.\d+)*[a-z]?(?:\s+R\d+)?)"
)
_CITES = _CITE + r"(?:\s*[,;+]\s*" + _CITE + r")*"
_SEP = r"(?:\s+—\s+|\s*[;,:]\s*)"
_PUBLIC_REWRITES = [
    # A story or epic folds into its milestone, and so does a dot
    # milestone: "M4.5-S04" and "M4.5" read "M4" (ADR-0166).
    (re.compile(r"\bM(\d+)(?:\.\d+)?-[A-Z]{1,4}\d+[a-z]?\b"), r"M\1"),
    (re.compile(r"\bM(\d+)\.\d+\b"), r"M\1"),
    (re.compile(r"\bS\d{1,2}[a-z]?\s+(?=[A-Za-z(])"), ""),
    (re.compile(r"\s*\(\s*" + _CITES + r"\s*\)"), ""),
    (re.compile(r"\(\s*" + _CITES + _SEP), "("),
    (re.compile(_SEP + _CITES + r"\s*\)"), ")"),
    (re.compile(r"\s*[;,]\s*" + _CITES + r"(?=\s*[;,])"), ""),
    (re.compile(r"\s+(?:since|in|per|by)\s+" + _CITES + r"\b"), ""),
    (re.compile(r"\bthe\s+" + _CITES + r"\s+"), "the "),
    (re.compile(r"\bthe [DA]\d+ (?=\S)"), "the "),
    (re.compile(r"\s*" + _CITES), ""),
    (re.compile(r"\(\s*\)"), ""),
    (re.compile(r"[ \t]{2,}"), " "),
    (re.compile(r"\s+([;,)])"), r"\1"),
]
_LEAK = re.compile(r"ADR-\d|F-L\d|FCR-|\bM\d+(?:\.\d+)?-[A-Z]{1,4}\d|\bM\d+\.\d")


def public_text(text):
    for pattern, replacement in _PUBLIC_REWRITES:
        text = pattern.sub(replacement, text)
    text = text.strip()
    if _LEAK.search(text):
        raise SystemExit(f"error: compat note still cites an unpublished record: {text!r}")
    return text


def md_inline(text):
    out = html.escape(text, quote=False)
    out = re.sub(r"`([^`]+)`", r"<code>\1</code>", out)
    return re.sub(r"\*\*([^*]+)\*\*", r"<strong>\1</strong>", out)


def parse_matrix(md):
    lines = md.splitlines()
    data = {"corpus": "", "surface": "", "rows": [], "deviations": []}
    i = 0
    while i < len(lines) and not lines[i].startswith("## Commands"):
        if lines[i].startswith("**Corpus:**"):
            data["corpus"] = lines[i].replace("**Corpus:**", "").strip().rstrip(".")
        elif lines[i].startswith("**Surface:**"):
            data["surface"] = lines[i].replace("**Surface:**", "").strip().rstrip(".")
        i += 1
    while i < len(lines) and not lines[i].startswith("| Command"):
        i += 1
    if i >= len(lines):
        raise SystemExit("error: command table not found in docs/compat-matrix.md")
    header = [c.strip().lower() for c in lines[i].strip().strip("|").split("|")]
    missing = [c for c in COLUMNS if c not in header]
    if missing or header[-1] != "notes":
        raise SystemExit(f"error: matrix header {header} lacks {missing} or does not end in Notes")
    at = {name: header.index(name) for name in COLUMNS}
    i += 2
    while i < len(lines) and lines[i].startswith("|"):
        cells = [c.strip() for c in lines[i].strip().strip("|").split("|")]
        if len(cells) < len(header):
            raise SystemExit(f"error: matrix row has {len(cells)} cells, header has {len(header)}: {lines[i]}")
        cells[len(header) - 1:] = ["|".join(cells[len(header) - 1:]).strip()]
        row = {name: cells[at[name]] for name in COLUMNS}
        row["command"] = row["command"].strip("`").strip()
        if row["status"] not in STATUS_ORDER:
            raise SystemExit(f"error: matrix row {row['command']!r} has unknown status {row['status']!r}")
        data["rows"].append(row)
        i += 1
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
    if not data["rows"]:
        raise SystemExit("error: no command rows parsed from docs/compat-matrix.md")
    return data


def compat_values():
    """The generated parts of the compatibility page, from the matrix."""
    data = parse_matrix(MATRIX.read_text(encoding="utf-8"))
    counts = {}
    for r in data["rows"]:
        counts[r["status"]] = counts.get(r["status"], 0) + 1
    total = len(data["rows"])
    stats = [f'<div class="stat"><div class="stat-v pix">{total}</div><div class="stat-l mono">commands declared</div></div>']
    stats += [f'<div class="stat"><div class="stat-v pix">{counts[s]}</div><div class="stat-l mono">{s}</div></div>'
              for s in STATUS_ORDER if counts.get(s)]
    chips = [f'<button type="button" class="chip chip-on" data-filter="all" aria-pressed="true">All · {total}</button>']
    chips += [f'<button type="button" class="chip" data-filter="{s}" aria-pressed="false">{s} · {counts[s]}</button>'
              for s in STATUS_ORDER if counts.get(s)]
    rows = []
    for r in data["rows"]:
        rows.append(
            f'<tr data-group="{r["status"]}"><td><code>{html.escape(r["command"])}</code></td>'
            f'<td><span class="st st-{r["status"]} mono">{r["status"]}</span></td>'
            f'<td class="mono">{html.escape(public_text(r["since"]))}</td><td class="mono muted">{html.escape(r["flags"]) or "—"}</td>'
            f'<td class="mono">{html.escape(r["arity"])}</td><td class="mono">{html.escape(r["cases"])}</td>'
            f'<td class="mono">{html.escape(r["evidence"])}</td>'
            f'<td class="note">{md_inline(public_text(r["notes"])) if r["notes"] else ""}</td></tr>')
    devs = []
    for cmd, bullets in data["deviations"]:
        items = "".join(f"<li>{md_inline(public_text(b))}</li>" for b in bullets)
        devs.append(f'<h3 class="dev-h mono">{html.escape(cmd)}</h3><ul class="dots">{items}</ul>')
    return {
        "compat_stats": "".join(stats),
        "compat_corpus": md_inline(public_text(data["corpus"])),
        "compat_surface": md_inline(public_text(data["surface"])),
        "compat_chips": "".join(chips),
        "compat_rows": "\n".join(rows),
        "compat_devs": "".join(devs),
    }


# ---------------------------------------------------------------------------
# Blog.


def load_posts():
    posts = []
    for p in POSTS:
        p = dict(p)
        if p["slug"]:
            path = SRC / "blog" / f"{p['slug']}.html"
            meta, body = front(path.read_text(encoding="utf-8"), path)
            p.update(meta)
            p["body"] = body
        posts.append(p)
    path = SRC / "blog" / f"{FEATURED['slug']}.html"
    meta, body = front(path.read_text(encoding="utf-8"), path)
    featured = dict(FEATURED, **meta, body=body)
    dates = [p["date"] for p in posts if p["slug"]]
    if dates != sorted(dates, reverse=True):
        raise SystemExit(f"error: POSTS must be newest first: {dates}")
    known = {p["slug"] for p in posts if p["slug"]} | {FEATURED["slug"]}
    orphans = sorted(p.stem for p in (SRC / "blog").glob("*.html") if p.stem not in known)
    if orphans:
        raise SystemExit(f"error: blog sources not in POSTS: {orphans}")
    return featured, posts


def avatar(size_cls):
    return f'<span class="avatar {size_cls} mono" aria-hidden="true">{AUTHOR[1]}</span>'


def build_blog_index(featured, posts):
    rel = "../"
    chips = "".join(
        f'<button type="button" class="chip{" chip-on" if t == "All" else ""}" data-filter="{"all" if t == "All" else html.escape(t)}" '
        f'aria-pressed="{"true" if t == "All" else "false"}">{html.escape(t)}</button>' for t in BLOG_TOPICS)
    rows = []
    for p in posts:
        live = '<span class="sq6 bg-sig" aria-hidden="true"></span>' if not p["slug"] else ""
        inner = (f'<div class="pr-date mono">{live}<span>{html.escape(p["date"])}</span></div>'
                 f'<div class="pr-cover">{cover_svg(p["kind"], p["seed"], 48, 27, 176, 98)}</div>'
                 f'<div class="pr-text"><div class="pr-title">{html.escape(p["title"])}</div>'
                 f'<p class="pr-ex">{html.escape(p["excerpt"])}</p></div>'
                 f'<div class="pr-topic mono">{html.escape(p["topic"])}</div>'
                 f'<div class="pr-meta-m mono">{live}{html.escape(p["date"] if p["slug"] else "Now writing · " + p["topic"].split(" · ")[0])}'
                 f'{"" if not p["slug"] else " · " + html.escape(p["group"])}</div>')
        if p["slug"]:
            rows.append(f'<a class="prow" href="{p["slug"]}.html" data-group="{html.escape(p["group"])}">{inner}</a>')
        else:
            rows.append(f'<div class="prow prow-live" data-group="{html.escape(p["group"])}">{inner}</div>')
    body = f'''<main id="main" class="wrap blog">
<header class="blog-head">
<div class="blog-title">
<div class="eyebrow">Blog</div>
<h1>Engineering notes.</h1>
<p>How InfinityDB is designed, measured and built, gate by gate.<span class="d-only"> Written as we build it.</span></p>
</div>
<div class="follow">
<label for="feed-url" class="eyebrow">New posts by RSS</label>
<div class="follow-row"><input id="feed-url" type="text" readonly value="{SITE_URL}blog/feed.xml" data-feed-url><button type="button" class="btn-ink" data-copy-target="#feed-url">Copy</button></div>
<a class="mono follow-link" href="feed.xml">RSS feed ↗</a>
</div>
</header>
<div class="chips chips-scroll" role="group" aria-label="Filter posts by topic" data-filter-group="#posts">{chips}</div>
<a class="featured" href="{featured["slug"]}.html">
<div class="feat-cover">{cover_svg("spiral", 1, 48, 27, 686, 386, "Pixel illustration: a vortex dissolving around a single signal pixel")}</div>
<div class="feat-text">
<div class="feat-kick mono"><span class="pill">Featured</span>{html.escape(featured["topic"])}</div>
<h2>{html.escape(featured["title"])}</h2>
<p>{html.escape(featured["excerpt"])}</p>
<div class="feat-foot"><div class="byline">{avatar("av32")}<span>{AUTHOR[0]}</span></div><span class="feat-read">Read the post →</span></div>
</div>
</a>
<div class="plist" id="posts">
<div class="plist-head mono" aria-hidden="true"><span>Date</span><span>Cover</span><span>Post</span><span>Topic</span></div>
{"".join(rows)}
</div>
</main>'''
    desc = "How InfinityDB is designed, measured and built, gate by gate."
    return page("Blog — InfinityDB", desc, "blog/index.html", body, active="blog")


def build_post(p, next_post, is_featured=False):
    rel = "../"
    body, toc = render_fragment(p["body"], "post-h2")
    body = body.replace(
        '<div class="law-label">', '<div class="law-label mono"><span class="sq6 bg-ink" aria-hidden="true"></span>')
    toc_html = "".join(
        f'<a href="#{hid}" data-toc="{hid}"{" class=on" if k == 0 else ""}><span class="sq5" aria-hidden="true"></span>{html.escape(t)}</a>'
        for k, (hid, t) in enumerate(toc))
    nxt = ""
    if next_post:
        nxt = (f'<a class="next-post" href="{next_post["slug"]}.html"><div><div class="mono pn-k">Next post</div>'
               f'<div class="next-t">{html.escape(next_post["title"])}</div></div><span aria-hidden="true">→</span></a>')
    label = ("Pixel illustration: a vortex dissolving around a single signal pixel" if is_featured else None)
    cov = cover_svg(p["kind"], p["seed"], 64, 24, 1196, 446, None)
    kicker = p.get("kicker", "")
    date_html = f'<span class="d-only dim">·</span><span class="d-only mono kicker">{html.escape(p["date"])}</span>' if p.get("date") else ""
    content = f'''<main id="main" class="wrap post">
<a class="back-m mono" href="index.html"><span aria-hidden="true">←</span>All posts</a>
<header class="post-head">
<div class="crumb mono"><a href="index.html">Blog</a><span>/</span><span class="ink">{html.escape(p.get("crumb", p.get("group", "")))}</span></div>
<div class="kicker-m mono">{html.escape(kicker)}</div>
<h1>{html.escape(p["title"])}</h1>
<p class="dek">{html.escape(p["dek"])}</p>
<div class="post-by">{avatar("av36")}<span class="ink">{AUTHOR[0]}</span><span class="d-only dim">·</span><span class="d-only mono kicker">{html.escape(kicker)}</span>{date_html}</div>
</header>
<figure class="post-cover"{f' aria-label="{label}"' if label else ' aria-hidden="true"'}>{cov}</figure>
<div class="post-grid">
<article class="post-body">
{body}
<div class="author">{avatar("av56")}<div class="author-t"><div class="author-n">{AUTHOR[0]}</div><div class="muted">Building InfinityDB.</div></div><a class="btn-xs btn-line mono" href="{AUTHOR[2]}">GitHub{ICON_EXT}</a></div>
{nxt}
</article>
<aside class="post-side">
<nav aria-label="On this page" class="post-toc"><div class="eyebrow">On this page</div><div class="toc">{toc_html}</div></nav>
<div class="side-card"><div class="eyebrow">The laws, in full</div><p>Each lesson became one of thirteen design laws. Read them with the code rules they produced.</p><a class="btn-sm btn-line" href="../docs/laws.html">Design laws →</a></div>
<button type="button" class="btn-xs btn-line mono copy-link" data-copy-url>{ICON_COPY}<span>Copy link</span></button>
</aside>
</div>
</main>'''
    desc = p.get("excerpt", p["dek"])
    return page(f'{p["title"]} — InfinityDB Blog', desc, f'blog/{p["slug"]}.html', content, active="blog")


def rss(featured, posts):
    items = []
    for p in [featured] + [p for p in posts if p["slug"]]:
        url = f"{SITE_URL}blog/{p['slug']}.html"
        date = ""
        if p.get("date"):
            d = datetime.date.fromisoformat(p["date"])
            date = f"<pubDate>{d.strftime('%a, %d %b %Y')} 00:00:00 +0000</pubDate>"
        items.append(f"<item><title>{html.escape(p['title'])}</title><link>{url}</link><guid>{url}</guid>"
                     f"{date}<description>{html.escape(p['excerpt'])}</description></item>")
    return ('<?xml version="1.0" encoding="utf-8"?>\n<rss version="2.0"><channel>'
            f"<title>InfinityDB engineering notes</title><link>{SITE_URL}blog/index.html</link>"
            "<description>How InfinityDB is designed, measured and built, gate by gate.</description>"
            + "".join(items) + "</channel></rss>\n")


# ---------------------------------------------------------------------------
# Small pages and assets.


def build_404():
    body = f'''<main id="main" class="wrap lost">
<div class="lost-moss">{moss_svg(240, 120, cls="sleep", label="Moss asleep")}<svg class="px zz" width="48" height="48" viewBox="0 0 8 8" aria-hidden="true"><path class="f-dim" d="M4 0h4v1h-4zM6 1h1v1h-1zM5 2h1v1h-1zM4 3h4v1h-4zM0 5h3v1h-3zM1 6h1v1h-1zM0 7h3v1h-3z"/></svg></div>
<div class="eyebrow">404 · Park</div>
<h1>Nothing lives at this address.</h1>
<p>Moss is waiting on the next completion. The page you asked for was never written, or it moved.</p>
<div class="btns"><a class="btn btn-ink" href="{SITE_URL}index.html">Home</a><a class="btn btn-line" href="{SITE_URL}docs/index.html">Docs</a></div>
</main>'''
    # GitHub Pages serves this file at any missing path, so every link is the
    # deployed site's absolute URL.
    return page("Not found — InfinityDB", "This page does not exist.", "404.html", body, rel=SITE_URL)


def sitemap(paths):
    urls = "".join(f"<url><loc>{SITE_URL}{p}</loc></url>" for p in sorted(paths) if p.endswith(".html") and p != "404.html")
    return ('<?xml version="1.0" encoding="utf-8"?>\n'
            f'<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">{urls}</urlset>\n')


HREF = re.compile(r'(?:href|src)="([^"]+)"')
IDS = re.compile(r'\bid="([^"]+)"')


def check_links(out):
    """Every internal link and fragment resolves inside the built site."""
    ids = {path: set(IDS.findall(text)) for path, text in out.items() if path.endswith(".html")}
    bad = []
    for path, text in out.items():
        if not path.endswith(".html"):
            continue
        base = path.rsplit("/", 1)[0] + "/" if "/" in path else ""
        for href in HREF.findall(text):
            if re.match(r"^[a-z]+:", href) or href.startswith("//"):
                continue
            target, _, frag = href.partition("#")
            if not target:
                resolved = path
            elif target.startswith("/"):
                bad.append(f"{path}: {href} (root-absolute: the site is served under a path)")
                continue
            else:
                parts = []
                for seg in (base + target).split("/"):
                    if seg == "..":
                        if parts:
                            parts.pop()
                    elif seg not in ("", "."):
                        parts.append(seg)
                resolved = "/".join(parts)
            if resolved not in out:
                bad.append(f"{path}: {href} (no such file)")
            elif frag and resolved in ids and frag not in ids[resolved]:
                bad.append(f"{path}: {href} (no id {frag!r})")
    if bad:
        raise SystemExit("error: broken internal links:\n  " + "\n  ".join(sorted(set(bad))))


def build():
    out = {}
    out["index.html"] = build_landing()
    build_docs(out)
    featured, posts = load_posts()
    out["blog/index.html"] = build_blog_index(featured, posts)
    real = [p for p in posts if p["slug"]]
    # The prologue reads into the first milestone, as the design's "Next post" does.
    m0 = next(p for p in real if p["slug"] == "architecture-first-the-m0-verdict")
    out[f"blog/{featured['slug']}.html"] = build_post(featured, m0, is_featured=True)
    for i, p in enumerate(real):
        nxt = real[i + 1] if i + 1 < len(real) else featured
        out[f"blog/{p['slug']}.html"] = build_post(p, nxt)
    out["blog/feed.xml"] = rss(featured, posts)
    out["404.html"] = build_404()
    for asset in sorted((SRC / "assets").iterdir()):
        out[f"assets/{asset.name}"] = asset.read_text(encoding="utf-8")
    out["assets/favicon.svg"] = favicon_svg()
    out["robots.txt"] = f"User-agent: *\nAllow: /\nSitemap: {SITE_URL}sitemap.xml\n"
    out["sitemap.xml"] = sitemap(out.keys())
    out[".nojekyll"] = ""
    for path, text in out.items():
        if path.endswith(".html") and "{{" in text:
            raise SystemExit(f"error: {path}: unreplaced token")
    check_links(out)
    return out


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--check", action="store_true", help="exit 1 if site/ differs from a fresh build")
    args = ap.parse_args()
    out = build()
    existing = {p.relative_to(OUT).as_posix() for p in OUT.rglob("*") if p.is_file()} if OUT.exists() else set()
    if args.check:
        stale = sorted(p for p, t in out.items() if not (OUT / p).exists() or (OUT / p).read_text(encoding="utf-8") != t)
        extra = sorted(existing - set(out))
        if stale or extra:
            for p in stale:
                print(f"stale: site/{p}")
            for p in extra:
                print(f"extra: site/{p}")
            print("error: website/site is out of date; run python3 website/build.py and commit the result")
            sys.exit(1)
        print(f"ok: site/ matches a fresh build ({len(out)} files)")
        return
    if OUT.name != "site" or OUT.parent != HERE:
        raise SystemExit(f"error: refusing to write outside website/site ({OUT})")
    for p in sorted(existing - set(out)):
        (OUT / p).unlink()
    for path, text in out.items():
        dest = OUT / path
        dest.parent.mkdir(parents=True, exist_ok=True)
        if not dest.exists() or dest.read_text(encoding="utf-8") != text:
            dest.write_text(text, encoding="utf-8")
    for d in sorted((p for p in OUT.rglob("*") if p.is_dir()), key=lambda p: -len(p.parts)):
        if not any(d.iterdir()):
            d.rmdir()
    print(f"wrote site/ ({len(out)} files)")


if __name__ == "__main__":
    main()
