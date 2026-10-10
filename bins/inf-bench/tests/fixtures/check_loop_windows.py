#!/usr/bin/env python3
"""Independently recompute native gate report windows from retained bucket pairs."""
import argparse
import re
from pathlib import Path

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("report", type=Path)
parser.add_argument("--cells", type=int, required=True)
parser.add_argument("--replicates", type=int, required=True)
args = parser.parse_args()
report = args.report.read_text()
pattern = (r"## loop histogram rep (\d+) cell (\d+) (before|after)\n\n"
           r"```[^\n]*\n([^\n]+)\ncounts=([0-9,]+)\n```")
snapshots = {}
for replicate, cell, phase, metadata, buckets in re.findall(pattern, report):
    key = (int(replicate), int(cell), phase)
    assert key not in snapshots, f"duplicate snapshot {key}"
    fields = dict(field.split("=", 1) for field in metadata.split())
    counts = list(map(int, buckets.split(",")))
    assert len(counts) == 1920
    assert sum(counts) == int(fields["samples"])
    assert int(fields["cell"]) == int(cell)
    assert int(fields["cells"]) == args.cells
    snapshots[key] = fields, counts
expected = {(r, c, p) for r in range(args.replicates)
            for c in range(args.cells) for p in ("before", "after")}
assert snapshots.keys() == expected, "missing or extra snapshot pairs"
percentiles = []
for replicate in range(args.replicates):
    for cell in range(args.cells):
        begin, before = snapshots[replicate, cell, "before"]
        end, after = snapshots[replicate, cell, "after"]
        assert begin["run_id"] == end["run_id"]
        counts = [b - a for a, b in zip(before, after)]
        assert min(counts) >= 0
        samples = sum(counts)
        assert samples > 0
        assert samples == int(end["samples"]) - int(begin["samples"])
        for name in ("submits", "sqes"):
            assert int(end[name]) >= int(begin[name])
        rank = samples - samples // 1000
        cumulative = 0
        for index, count in enumerate(counts):
            cumulative += count
            if cumulative >= rank:
                percentile = index if index < 32 else ((33 + index % 32) << (index // 32 - 1)) - 1
                break
        note = (f"loop window rep {replicate} cell {cell}: {samples} samples, "
                f"p999 {percentile} us")
        assert note in report, f"window note differs: {note}"
        percentiles.append(percentile)
row = re.search(r"\| (?:Reactor )?[Ll]oop iteration p99\.9 \|[^|]+\| ([0-9.]+) \|", report)
assert row, "missing loop gate row"
assert float(row[1]) == max(percentiles), "gate is not the worst cell/window"
print(f"verified {len(percentiles)} cell windows; worst bucket p99.9 {max(percentiles)} us")
