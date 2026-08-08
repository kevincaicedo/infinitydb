# M4-S04 steel-thread cold-read histogram — reference box (risk-gate input)

date: 2026-07-18 · HomeLab reference box (ADR-0022 D1) · ADATA LEGEND
700 Gen3 DRAM-less NVMe (§19 deviation disclosed) · ext4 ·
governor/EPP performance, turbo off · taskset -c 4 ·
INF_STEEL_DIR=$HOME/.cache/inf-steel · fadvise-DONTNEED per read ·
includes pump overhead · idle-drain shape (informational by declaration;
S22's loaded zipfian rows own the gate verdict)

Command: `INF_STEEL_DIR=$HOME/.cache/inf-steel taskset -c 4 cargo test
-p inf-runtime --features uring --release -- --ignored
cold_read_histogram --nocapture` — 3 pinned replicates, 300 rounds each:

  rep 1: p50 155.5 us | p90 157.2 us | p99 162.6 us | max 244.5 us
  rep 2: p50 165.4 us | p90 167.0 us | p99 198.9 us | max 299.4 us
  rep 3: p50 164.9 us | p90 166.9 us | p99 174.2 us | max 252.9 us

Risk-gate read: steel-thread cold-read p99 = 163–199 µs on the
reference NVMe at prototype QD — 7.5× inside the < 1.5 ms bound.
