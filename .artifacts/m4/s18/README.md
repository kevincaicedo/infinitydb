# M4-S18 — Extent reclaim + compaction interplay (dev-tier evidence)

Suite: `crates/inf-store/tests/tiered_blob_reclaim.rs`
Run: `cargo test -p inf-store --release --test tiered_blob_reclaim -- --test-threads=1 --nocapture`
Box: Linux dev box (dev-tier per the box profile; no claim-ledger row).

## leak-interplay-release-20260730.txt

- **Leak test (plan AC 1, 10⁶ cycles):** 1,000,000 blob
  create/overwrite/delete cycles with the compaction slice and the
  reclaim slice interleaved in the same MAINTAIN round — 687,454
  extents created, **687,454 reclaimed** (equality asserted), 15,626
  reclaim slices, extent directory == live set (empty) at quiescence,
  zero `.iblob` bytes lingering on the (mem) filesystem, reclaim
  backlog 0. Debug builds run a scaled 100k cycles (stated in the test
  docstring; the release run is the AC row).
- **statvfs leg (the plan's statfs assert):** real filesystem
  (`StdSegmentFs`), 768 cycles of ~24 KiB extents — peak 7,012,352
  bytes of `.iblob` on disk, all unlinked at the VFS
  (`blob_dir_bytes == 0` after quiesce), `statvfs` available-bytes
  back to baseline within slack. Reduced cycle count relative to the
  mem run, stated, not hidden: the 10⁶-cycle exactness row is the mem
  run above; this leg proves the filesystem half.
- **References move, blob bytes never:** across a full compaction pass,
  `blob_bytes` delta = 0, every live extent's file image byte-identical,
  the extent directory untouched, and the relocation volume equals the
  record legs exactly (512-byte payloads would be several times the
  asserted bound).
- **No starvation:** with a 384-extent reclaim backlog and interleaved
  copy-forward work standing together, bounded per-round budgets
  (compaction 16 KiB, reclaim 4 unlinks) drain both; any round that
  reclaimed nothing against a standing durable backlog fails the test.

The WA report split (plan AC 2) is code + unit-level evidence:
`WriteAccounting::blob_write_amplification` (milli, ceiling, saturating
— the S16 rounding discipline on the ADR-0061 D8 counters),
`INFO tiering` per-ns `blob_write_amp_milli=` + cell
`tiering_blob_write_amp_milli_max`/`_undefined_ns`/`_reclaimable`/
`_reclaim_deferred`, and `inf-bench`'s `writeamp::blob_disposition`
(pair-asserted against the raw counters, rendered beside the record
disposition in every m4 row). The block-layer (`/proc/diskstats`)
re-read with extents live joins the S22/S24 campaign rows.
