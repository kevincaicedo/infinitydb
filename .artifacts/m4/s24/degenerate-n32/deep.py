#!/usr/bin/env python3
"""Deeper read of the salvaged n=32 legs: bucket geometry, slot asymmetry,
and a bootstrap of the pooled-median gate statistic against the A/A null."""
import random
import re
from collections import Counter
from pathlib import Path

random.seed(20260816)  # deterministic; no ambient randomness in evidence

ROW_RE = re.compile(r"^== row: (.+?) \(crossover")
REP_RE = re.compile(r"^\s+rep (\d+) (m4|m3-baseline): (\d+) ops/s, p999 (\d+) µs")


def median(v):
    s = sorted(v)
    return s[len(s) // 2]


def parse(path):
    rows, order, cur = {}, {}, None
    for line in Path(path).read_text().splitlines():
        m = ROW_RE.match(line)
        if m:
            cur = m.group(1)
            rows[cur] = {"m4": {}, "m3-baseline": {}}
            order[cur] = []
            continue
        m = REP_RE.match(line)
        if m and cur:
            rep, who = int(m.group(1)), m.group(2)
            rows[cur][who][rep] = (int(m.group(3)), int(m.group(4)))
            order[cur].append((rep, who))
    return rows, order


def slots(order_row):
    first = {}
    for rep, who in order_row:
        first.setdefault(rep, who)
    return first


def report(tag, path):
    rows, order = parse(path)
    print(f"\n{'#'*76}\n# {tag}\n{'#'*76}")
    res = {}
    for row, d in rows.items():
        first = slots(order[row])
        m4 = {r: v[1] for r, v in d["m4"].items()}
        bs = {r: v[1] for r, v in d["m3-baseline"].items()}
        reps = sorted(m4)

        vals = sorted(set(list(m4.values()) + list(bs.values())))
        steps = sorted({b - a for a, b in zip(vals, vals[1:])})
        width = steps[0]
        centre = median(list(m4.values()))
        print(f"\n=== {row}")
        print(f"  bucket width here: {width} µs at ~{centre} µs  ->  "
              f"one bucket = {width/centre*100:.2f}%  (1% bar is {width/centre*100/1:.1f} "
              f"buckets wide -> UNREACHABLE)" if width / centre * 100 > 1
              else f"  bucket width {width} µs")

        # pooled (what the gate uses)
        a, b = median(list(bs.values())), median(list(m4.values()))
        print(f"  POOLED gate  base {a} -> m4 {b}  = {(b-a)/a*100:+.2f}%  "
              f"({(b-a)//width:+.0f} buckets)")

        # slot-matched
        for slot in ("first", "second"):
            mm = [m4[r] for r in reps if (first[r] == "m4") == (slot == "first")]
            bb = [bs[r] for r in reps if (first[r] == "m3-baseline") == (slot == "first")]
            am, bm = median(bb), median(mm)
            print(f"  slot={slot:<6} n={len(mm)}  base {am} -> m4 {bm} = {(bm-am)/am*100:+.2f}% "
                  f"({(bm-am)//width:+.0f} buckets)")

        # distribution
        print(f"  m4   hist: {dict(sorted(Counter(m4.values()).items()))}")
        print(f"  base hist: {dict(sorted(Counter(bs.values()).items()))}")

        # bootstrap the pooled statistic by resampling REPS (keeps pairing)
        boot = []
        for _ in range(20000):
            pick = [random.choice(reps) for _ in reps]
            aa = median([bs[r] for r in pick])
            bb2 = median([m4[r] for r in pick])
            boot.append((bb2 - aa) / aa * 100)
        boot.sort()
        lo, hi = boot[int(0.025 * len(boot))], boot[int(0.975 * len(boot))]
        frac_pos = sum(1 for x in boot if x > 0) / len(boot)
        print(f"  bootstrap pooled delta: 95% CI [{lo:+.2f}%, {hi:+.2f}%]  "
              f"P(m4 worse) = {frac_pos:.2f}")
        res[row] = dict(width=width, pooled=(b - a) / a * 100, ci=(lo, hi), fp=frac_pos)
    return res


base = Path(__file__).resolve().parent
            
ab = report("A/B — m4 6bd25b1  vs  m3 baseline a1ebcb9", base / "ab-transcript.txt")
aa = report("A/A — m4 6bd25b1  vs  m4 6bd25b1  (IDENTICAL BINARIES = null)", base / "aa-transcript.txt")

print(f"\n{'='*76}\nSUMMARY: is any A/B p99.9 shift bigger than the same-binary null?\n{'='*76}")
print(f"{'row':<34} {'A/B':>18} {'A/A (null)':>18}")
for r in ab:
    print(f"{r:<34} {ab[r]['pooled']:+6.2f}% [{ab[r]['ci'][0]:+.1f},{ab[r]['ci'][1]:+.1f}] "
          f"{aa[r]['pooled']:+6.2f}% [{aa[r]['ci'][0]:+.1f},{aa[r]['ci'][1]:+.1f}]")
