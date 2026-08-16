#!/usr/bin/env python3
"""Reproduce inf-bench gate-run m4 degenerate_row math from a salvaged transcript.

Harness reference (bins/inf-bench/src/):
  gaterun.rs:227  median(v) = sorted(v)[len(v)//2]      # UPPER median
  m2rows.rs:41    delta_pct(a,b) = (b-a)/a*100          # a=baseline, b=m4
  m4rows.rs:148   p999_signed = delta_pct(a_p999, b_p999); gate = max(0, signed)
"""
import re
import sys
from pathlib import Path

ROW_RE = re.compile(r"^== row: (.+?) \(crossover")
REP_RE = re.compile(r"^\s+rep (\d+) (m4|m3-baseline): (\d+) ops/s, p999 (\d+) µs")


def median(vals):
    s = sorted(vals)
    return s[len(s) // 2]


def parse(path):
    rows = {}
    order = []  # (row, rep, who) in transcript order -> gives slot
    cur = None
    for line in Path(path).read_text().splitlines():
        m = ROW_RE.match(line)
        if m:
            cur = m.group(1)
            rows[cur] = {"m4": {}, "m3-baseline": {}}
            order.append((cur, []))
            continue
        m = REP_RE.match(line)
        if m and cur:
            rep, who, ops, p999 = int(m.group(1)), m.group(2), int(m.group(3)), int(m.group(4))
            assert rep not in rows[cur][who], f"dup {cur} rep{rep} {who}"
            rows[cur][who][rep] = (ops, p999)
            order[-1][1].append((rep, who))
    return rows, dict(order)


def analyze(name, path):
    rows, order = parse(path)
    print(f"\n{'='*74}\n{name}  ({path.name})\n{'='*74}")
    out = {}
    for row, data in rows.items():
        n4, nb = len(data["m4"]), len(data["m3-baseline"])
        assert n4 == nb == 32, f"{row}: parsed {n4}/{nb}, expected 32/32"
        m4p = [v[1] for v in data["m4"].values()]
        bsp = [v[1] for v in data["m3-baseline"].values()]
        m4o = [v[0] for v in data["m4"].values()]
        bso = [v[0] for v in data["m3-baseline"].values()]

        a_p, b_p = median(bsp), median(m4p)
        a_o, b_o = median(bso), median(m4o)
        d_p = (b_p - a_p) / a_p * 100
        d_o = (b_o - a_o) / a_o * 100

        # slot analysis: within each rep, who ran first
        first = {}
        for rep, who in order[row]:
            first.setdefault(rep, who)
        s1 = [data[first[r]][r][1] for r in sorted(first)]           # ran first
        s2 = [data["m4" if first[r] == "m3-baseline" else "m3-baseline"][r][1]
              for r in sorted(first)]                                 # ran second
        m4_first = sum(1 for r in first if first[r] == "m4")

        out[row] = dict(gate_p999=max(0.0, d_p), signed_p999=d_p,
                        a_p=a_p, b_p=b_p, gate_ops=max(0.0, -d_o), signed_ops=d_o)

        print(f"\n-- {row}")
        print(f"   n=32/32  m4-first in {m4_first}/32 reps (crossover balanced: {m4_first==16})")
        print(f"   p999 median  base {a_p:>6} -> m4 {b_p:>6} µs   signed {d_p:+7.2f}%   "
              f"GATE {max(0.0,d_p):6.2f}%")
        print(f"   ops  median  base {a_o:>8} -> m4 {b_o:>8}      signed {d_o:+7.2f}%   "
              f"GATE {max(0.0,-d_o):6.2f}%")
        print(f"   slot: ran-FIRST median {median(s1):>6} µs | ran-SECOND median "
              f"{median(s2):>6} µs  (second/first {median(s2)/median(s1):.2f}x)")
        # distribution shape
        uniq = sorted(set(m4p + bsp))
        print(f"   p999 distinct values across both legs (n=64): {len(uniq)} -> {uniq[:12]}"
              f"{' ...' if len(uniq)>12 else ''}")
        srt4, srtb = sorted(m4p), sorted(bsp)
        print(f"   m4   sorted[14:19] = {srt4[14:19]}   (median idx16 = {srt4[16]})")
        print(f"   base sorted[14:19] = {srtb[14:19]}   (median idx16 = {srtb[16]})")
    return out


ab = analyze("A/B  m4(6bd25b1) vs m3-baseline(a1ebcb9)", Path(sys.argv[1]))
aa = analyze("A/A  m4(6bd25b1) vs m4(6bd25b1)  IDENTICAL BINARIES", Path(sys.argv[2]))

print(f"\n{'='*74}\nVERDICT TABLE  (threshold: ops/RSS 1.0%; p999 rows re-expressed 0.0% "
      f"= same bucket or better, ADR-0070 D4b)\n{'='*74}")
print(f"{'row':<34} {'A/B p999':>10} {'A/A p999':>10} {'A/B ops':>9} {'A/A ops':>9}")
for row in ab:
    print(f"{row:<34} {ab[row]['gate_p999']:>9.2f}% {aa[row]['gate_p999']:>9.2f}% "
          f"{ab[row]['gate_ops']:>8.2f}% {aa[row]['gate_ops']:>8.2f}%")
