# M4-S17 — Out-of-line blob extents + reference counting (2026-07-30, dev tier)

ADR-0061. All three plan ACs closed with named artifacts; one design
discovery found and fixed by the DST sweep before merge.

## AC evidence

- **AC1 — extent↔frame cut**: `tests/crash-matrix/tests/blob.rs` carries
  the four `m4.toml` S17 rows. `orphan_cut_reclaims_never_serves_and_the_
  referenced_twin_serves` drives both halves of the ordering rule in one
  recovery unit: the durable-but-unreferenced extent is swept (never
  resolved by any read path), the referenced twin serves byte-exact
  through the CRC-verified reader. `blob_write_faults_abandon_the_extent_
  typed` (short write + fsync abort → typed, id quarantined, debris
  swept) and `blob_unlink_failure_defers_nonfatally_and_the_retry_
  reclaims` carry the other verdicts.
- **AC2 — 1 GiB round trip, staging ≤ 2× chunk budget**:
  `blob-roundtrip-1gib-20260730.md` — 3 replicates on real NVMe
  (`Direct`, taskset -c 4): peak staging 1,056,768 B vs the 2,097,152 B
  budget (0.50×, identical across replicates — the bound is structural),
  blob WA 1.001× (device 1,074,798,592 B / value 1,073,741,824 B). The
  bench asserts both bounds in-process per chunk (`inf-log/benches/
  blob_roundtrip.rs`); a staging regression fails the command.
- **AC3 — refcount storms**: `inf-store/tests/tiered_blob.rs` — a 6,000-op
  deterministic storm + a proptest arm over mixed inline/blob writes,
  overwrites both directions, deletes, demotion, compaction relocations,
  and the stamped reclaim queue actually unlinking MemFs files. Oracles:
  refcounts equal the model at every checkpointed step; an unlink
  candidate is never model-live (zero early frees); at quiescence the
  extent directory equals the live set exactly (zero leaks); the
  recovery round trip (tag-9 images + 0x03 + 0x04 + 0x05 + tail replay +
  orphan sweep) reconciles exactly and serves blob content byte-exact.

## DST

`recovery-blob-sweep-20260730/`: `m4-recovery` grew the blob leg
(SetBlob ops with real cut physics, seeded durable orphans — the AC1 cut
1-in-48 ops, in-life reclaim slices, the post-recovery refcount oracle +
boot sweep). **3,000 seeds, 0 violations** — 1,233,233 blobs written,
120,006 orphans planted, 1,260,334 extents reclaimed, 465,593
relocations in the same runs. Smoke seed 0xC0FFEE: determinism verified
(`just sim-smoke`).

**Design discovery (the sweep's catch, 329/3000 seeds pre-fix):** a park
latched at a transient replay zero must revoke at re-registration — a
mid-walk blob SET is captured twice (fuzzy-walk tag-9 image + tail
re-coverage), and the displace-then-reapply pairing dips the refcount to
zero between the two; the stale park let the boot sweep hand a **live**
extent to the unlink slice. Fixed in `ExtentRefs::register` (queue
retraction — the D4-rule-3 at-least-once physics applied to the reclaim
queue); corollary: `unlink_extent_file` treats already-absent as success
(a replayed death legitimately re-offers a prior-life unlink). Recorded
in ADR-0061 D5.

Also fixed in-story: `ExtentReader::read` composed wrongly across
streamed chunks (`tier_extract` replaces its output; the reader now
extracts per frame straight into the caller's buffer — which is also
what keeps read staging to one window). Caught by the AC1 crash row;
regression-pinned in `blob.rs` unit tests (`streamed reads compose`).

## Environment

Linux dev box per the standing profile (ADATA LEGEND 700 Gen3 DRAM-less
NVMe — disclosed; replicate 3's write-throughput dip is the documented
sustained-write collapse, AC terms unaffected). Dev tier: no claim-ledger
row is asserted by this story.
