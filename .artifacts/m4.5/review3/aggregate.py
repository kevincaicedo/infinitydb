#!/usr/bin/env python3
"""Campaign B aggregation: per pairing, medians across the per-round reports
(one replicate each) of the predeclared quantities — G1 p50/barrier, G2
barrier 4c/1c, G3 client 4c/1c, tails, reads — from the raw rows."""
import glob, os, re, statistics as st, sys
root = os.path.expanduser('~/bench-data/s35-gate/artifacts-review3')
rx = re.compile(r'([\w/]+)=([0-9.]+)')
def rows(report):
    out = {}
    for line in open(report):
        m = re.match(r'^rep0 (4c|1c) (c32|c256|read) ', line)
        if not m: continue
        key = m.group(1) + ' ' + m.group(2)
        kv = dict((k, float(v)) for k, v in rx.findall(line))
        # '4c/1c: p50=1.420 barrier=1.391' is on the 1c line
        m2 = re.search(r'4c/1c: p50=([0-9.]+) barrier=([0-9.]+)', line)
        if m2:
            kv['r_client'] = float(m2.group(1)); kv['r_barrier'] = float(m2.group(2))
        out[key] = kv
    return out
def med(v): return st.median(v) if v else float('nan')
def spread(v): return f"{min(v):.3f}–{max(v):.3f}" if v else "—"
for pairing in ['k1s4', 'k3s2', 'k3s4']:
    reps = []
    for d in sorted(glob.glob(f'{root}/B-s35-{pairing}-r*')):
        r = glob.glob(f'{d}/*/report.md')
        if r: reps.append((os.path.basename(d), rows(r[0])))
    if not reps:
        print(f'{pairing}: no reports yet'); continue
    g1 = [r['4c c32']['p50_us'] / r['4c c32']['barrier_p50_us'] for _, r in reps if '4c c32' in r]
    g2 = [r['1c c32']['r_barrier'] for _, r in reps if '1c c32' in r]
    g3 = [r['1c c32']['r_client'] for _, r in reps if '1c c32' in r]
    p99 = [r['4c c32']['p99_us'] for _, r in reps if '4c c32' in r]
    p50 = [r['4c c32']['p50_us'] for _, r in reps if '4c c32' in r]
    one = [r['1c c32']['p50_us'] for _, r in reps if '1c c32' in r]
    b4 = [r['4c c32']['barrier_p50_us'] for _, r in reps if '4c c32' in r]
    b1 = [r['1c c32']['barrier_p50_us'] for _, r in reps if '1c c32' in r]
    ops = [r['4c c32']['ops/s'] for _, r in reps if '4c c32' in r]
    mx256 = [r['4c c256']['max_us'] for _, r in reps if '4c c256' in r]
    ops256 = [r['4c c256']['ops/s'] for _, r in reps if '4c c256' in r]
    rd = [r['4c read']['ops/s'] for _, r in reps if '4c read' in r]
    flagged = [n for n, r in reps if any(r[k]['barrier_p99_us'] > 10000 for k in r if k != '4c read')]
    print(f'== {pairing} ({len(reps)} reps; drive-state flagged: {flagged or "none"})')
    print(f'  G1 p50/barrier      median {med(g1):.3f}  spread {spread(g1)}   (4c p50 {med(p50):.0f} µs, barrier {med(b4):.0f})')
    print(f'  G2 barrier 4c/1c    median {med(g2):.3f}  spread {spread(g2)}   (1c barrier {med(b1):.0f})')
    print(f'  G3 client 4c/1c     median {med(g3):.3f}  spread {spread(g3)}   (1c p50 {med(one):.0f})')
    print(f'  4c c32 ops/s {med(ops):.0f}  p99 median {med(p99):.0f} spread {spread(p99)}  c256 ops/s {med(ops256):.0f} max {spread(mx256)}')
    print(f'  read ops/s median {med(rd):.0f}  spread {spread(rd)}')
