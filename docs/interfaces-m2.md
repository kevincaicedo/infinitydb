# M2 interface freezes — log spine

Companion to `interfaces-m0.md`, same contract: these interfaces froze at
**M2 exit**; changing a frozen one afterwards requires an ADR, and the
sections below carry each change with the decision that made it. Status
column tracks arrival.

Formats defined by ADR-0011 unless noted.

| Interface | Crate | Status |
|-----------|-------|--------|
| Log record format v1 | `inf-log` | implemented (M2-S01) |
| Batch frame layout v1 | `inf-log` | implemented (M2-S01); **read-only since M2.5-S12** (ADR-0031 — v2 is the written format; v1 accepted forever on the alpha line) |
| Batch frame layout v2 (per-frame sequencing) | `inf-log` | implemented (M2.5-S12, ADR-0031 — epoch · seq · covered-LSN stamp under the CRC; the tail-scan attestation taxonomy consumes it) |
| Batch frame layout v3 (aligned successor) | `inf-log` | implemented (M4.5-S34, ADR-0086 D3 — the v2 layout under `IFR3`; successor at the next 4 KiB boundary, zero padding written by the frame; written on `Direct` segments, v2 stays the buffered format) |
| LSN addressing | `inf-log` | implemented (M2-S01) |
| Segment naming + lifecycle | `inf-log` | implemented (M2-S02) |
| `SegmentFs` injection seam | `inf-log` | implemented (M2-S02; extended by S05/S11/S16; sim-disk tier at S18) |
| `MutationEffect` → record seam | `inf-store` → `inf-log` | implemented (M2-S03/S08, ADR-0012/0015; dep edge + command-layer post-image emission live) |
| Document record classes + replay contract | `inf-doc`/`inf-store`/`inf-log` | implemented (M3-S17, ADR-0043 — tags 6/7, incarnation lineage, recorded replay witnesses, static opcode registry, modular-u24 replay, checkpoint full images) |
| Log staging domain (`StagingRing`) | `inf-log` | implemented (M2-S03; reactor wiring at S05) |
| Sequential read path (`SegmentReader`) | `inf-log` | implemented (M2-S04; `ReadStep`/`next_step` make the clean end explicit; S14 tail policy — ADR-0018, foreign-segment classification — ADR-0090 D2) |
| Durability watermark contract | `inf-log`/`inf-runtime` | implemented (M2-S05/S06/S08, ADR-0013/0015; live on `ServerPlane` — acks seq-keyed, semantics unchanged; sim disk at S18) |
| Driver file ops (`LogWrite`/`Fdatasync`) | `inf-runtime` | implemented (M2-S05, ADR-0013 D1 — extends the frozen M0 `BackendDriver` contract) |
| Node catalog `META` swap (`inf-log::meta`) | `inf-log` | implemented (M2-S08, ADR-0015 D3 — the S11 MANIFEST protocol class; payload = `inf-store::catalog` v1 → v2 (ADR-0062 D6) → v3 (ADR-0075 D2) → **v4 (ADR-0100 D1, 2026-09-01)**: a drop-tombstone section, written iff non-empty, owned by the catalog writer) |
| Namespace selection + `Op::ApplyNs` | `inf-server`/`inf-fabric` | implemented (M2-S08, ADR-0015 D1 — additive opcode 6; `ns ≥ 16` enforced at decode; `program: bool` and header bit 0 carry execution origin under ADR-0115 D2–D5, as specified in `interfaces-m0.md` §4) |
| `.ick` checkpoint format v1 | `inf-log` | implemented (M2-S10, ADR-0016 — record-v1 payload; digest = hash64 chain, recorded deviation from "xxh3") |
| Checkpoint scheduler group + ckpt token classes | `inf-runtime` | implemented (M2-S10/S11, ADR-0016 D4/D5 + ADR-0017 D3 — `GroupClass::Checkpoint`; `TokenClass::{CkptWrite,CkptSync,ManifestSync}` routing-only extensions of the frozen M0 token contract) |
| MANIFEST schema (epoch 3) | `inf-log` | implemented (M2-S11, ADR-0017 — `INFMAN1\0` payload in the META envelope class; swap steps ride the driver via `TokenClass::ManifestSync` on the reactor tier). Epoch 2 added tier sections (M4-S12, ADR-0057 D5); **epoch 3 (ADR-0094 D6, 2026-08-30)** adds `key_hash_id: u64` after `begin` and makes `tier_ns_count` always present — the manifest names the key-hash secret that placed its checkpoint's refs, and recovery compares before the checkpoint loads. Epochs 1/2 decode to the typed `PredatesKeyHashBinding` (no migration) |
| Tail-region scan + corruption taxonomy | `inf-log::tail` | implemented (M2-S14, ADR-0018 — `scan_region`/`RegionScan` facts + `LogCorruption`; policy in `inf-server::recover`) |
| Recovery state digest | `inf-store` | implemented (M2-S13, ADR-0018 — `Keyspace::state_digest`, order-independent multiset hash; the S18/S19 oracle currency) |
| Fault-point registry | `inf-foundation`/`inf-log`/`inf-server` | implemented (M2-S16/S17, ADR-0019/0020 — thread-local, feature-gated `fault-points`, deterministic triggers; 8 points wired + CI inventory check across all decl modules) |
| Crash matrix (definition-as-data + runner) | `tests/crash-matrix` | implemented (M2-S17, ADR-0020 — `m2.toml` rows × policies × workloads × seeds, kill-and-recover on `MemFs`; fsyncgate child-process row; per-PR via workspace tests + expanded-seed nightly) |
| Sim disk (lose/tear/reorder) | `inf-log::fs::sim` | implemented (M2-S18, ADR-0020 — `SimDisk`: volatile-until-fsync data, dir-fsync-ordered metadata, seeded sector-granular power cuts; driver ops executed by the `inf-sim` `SimDriver`) |
| Durable plane over `SegmentFs` (generic) | `inf-server` | implemented (M2-S19, ADR-0021 D1 — `ServerPlane<O, F = StdSegmentFs>`, `DurableCell/CkptCell/ManifestCell/BootRecovery<F>`; `begin_recovery(fs, …)`; monomorphized, zero std-tier change) |
| Detached control plane (`ControlInbox`) | `inf-server` | implemented (M2-S19, ADR-0021 D2 — `ControlHandle::detached[_with_catalog]` + inline `drain(fs, dir)`: catalog swaps + unlinks inside the deterministic sim loop; `load_catalog_from<F>`) |
| Durability oracle + sweep (`m2-durable`) | `inf-sim` | implemented (M2-S19, ADR-0021 D3-D5 — ack-stream oracle incl. the survival audit on taxonomy-refused boots; `--sweep/--shard/--out`; 10k-seed gate artifact green; `Plant::FsyncLies` canary) |
| Checkpoint board (`CkptBoard`/`CkptSlot`) | `inf-server::control` | implemented (M2-S20, ADR-0021 D6 — per-cell request/publication epoch slots; publication at the MANIFEST swap's dir-fsync commit; aborted swaps retry after backoff so `WAIT` cannot hang. ADR-0159 A1: an all-cell request word beside the slots, requests issued only by partition credits, cells observe the board through a bounded sweep) |
| `INF.CKPT`/`BGSAVE`/`LASTSAVE` | `inf-wire`/`inf-server` | implemented (M2-S20 — registry grew to 68 (hash multipliers re-searched); pump-routed `program_ckpt`; compat entries + deviations declared) |
| Persistence counter set | `inf-log`/`inf-server`/`inf-bench` | implemented (M2-S21, ADR-0021 D7 — `INFO persistence` gains rates/percentiles/ages; the acks-per-fsync grouping tripwire is report-enforced in `gate-run m2` with a canary mode) |

## Record format v1 (`inf-log::record`)

```text
record := varint(body_len) body
body   := type: u8 · flags: u8 · varint(ns) · payload
```

- Varints: canonical LEB128 (`inf-foundation::varint`); non-minimal
  encodings are decode errors — one value, one encoding (L7).
- Type tags (wire format — never reused or renumbered):
  `1 StringPostImage {varint klen · key · value}` ·
  `2 Delete {key}` ·
  `3 ExpireAt {varint unix-ms · key}` (absolute time) ·
  `4 NsOp {opaque payload}` (vocabulary owned by M2-S08) ·
  `5 CkptBegin {varint ckpt-id}` (cell-scoped; outer `ns = 0`) ·
  `6 DocDelta {varint klen · key · lineage:u64 LE ·
  base-version:u24 LE · match-count:u32 LE · post-len:u24 LE ·
  opcode:u8 · varint program-len · program · operand}` ·
  `7 DocFull {varint klen · key · lineage:u64 LE · version:u24 LE ·
  idoc}`.
  The outer record carries `ns` for tags 6/7. Document opcodes 1–13 and
  operand canonicality are owned by `inf-doc::delta`, not the log spine
  (ADR-0043 D3/D4). `lineage` is nonzero. Delta `match-count` and
  `post-len` are nonzero exact live-acceptance witnesses; zero is a typed
  decode error.
- `flags`: reserved, v1 defines no bits. Unknown flags and unknown types
  are **fail-stop** decode errors — replay refuses, never skips.
- Public surface: `RecordView<'_>` (borrowing views; invalid records are
  unrepresentable), `RecordView::encode_into(&mut Vec<u8>)` /
  `encoded_len()`, `decode_record(&[u8]) -> (RecordView, consumed)`,
  `NsId(u32)`, `DocLineage(NonZeroU64)`, `RecordDecodeError`.

## Batch frame layout v1 (`inf-log::frame`)

```text
offset size field
0      4    magic = "IFR1"        (all-zero magic ⇒ preallocated tail)
4      4    frame_len: u32 LE     (total: header+body+trailer; ≥ 28)
8      4    record_count: u32 LE  (≥ 1 — empty iterations emit no frame)
12     8    first_lsn: segment u32 LE · offset u32 LE   (first RECORD's LSN)
20     …    body: records
len-4  4    CRC32C(header·body): u32 LE   (kernel: inf-simd::crc32c)
```

- One frame per loop iteration (L3). Replay validates the CRC, then
  applies per frame; record-level errors inside a CRC-valid frame are
  corruption-or-bug and fail-stop.
- Public surface: `FrameBuilder` (reusable buffer; `append(&RecordView)`,
  `frame_len()`, `finalize(first_record_lsn, stamp) -> &[u8]`,
  `sealed_frame()` (post-finalize re-access for the leased in-flight
  write), `reset()`), `decode_frame(&[u8], max_frame_len)` (both formats),
  `FrameRef::records()` yielding `(Lsn, RecordView)`, `FrameRef::stamp()`,
  `FrameIter` (validate-then-yield; stops at zero magic; `offset()` =
  bytes consumed — the tail-scan input for S04/S14), `FrameDecodeError`,
  `FrameRecordError`.

## Batch frame layout v2 (`inf-log::frame`, M2.5-S12 — ADR-0031)

```text
offset size field
0      4    magic = "IFR2"        (all-zero magic ⇒ preallocated tail; "IFR1" ⇒ v1, read-only)
4      4    frame_len: u32 LE     (total: header+body+trailer; ≥ 48)
8      4    record_count: u32 LE  (≥ 1)
12     8    first_lsn: segment u32 LE · offset u32 LE   (first RECORD's LSN — base + 40)
20     4    epoch: u32 LE         (log life; ≥ 1 — 0 is a decode error)
24     8    seq: u64 LE           (frame ordinal within the epoch, from 1; 0 is a decode error)
32     8    covered_lsn: u64 LE   (Lsn::to_u64 of the durability watermark at seal;
                                   > first_lsn is a decode error — the watermark never
                                   leads the append cursor)
40     …    body: records
len-4  4    CRC32C(header·body): u32 LE
```

- The stamp is what recovery's evidence taxonomy consumes (ADR-0031
  D3–D5): prefix continuity (epoch nondecreasing; seq +1 within an epoch
  and within a segment; seq 1 at an epoch step; `covered_lsn`
  nondecreasing within a run), the beyond-the-data-end **attestation
  rule** (a surviving frame attesting coverage past the data end → named
  refusal; otherwise the gap is provably un-covered and truncates — the
  retired ADR-0021 D3 refusal class, counted in
  `RecoverStats::beyond_frames_discarded`), the **cross-segment
  attestation check** (coverage claims must lie within earlier segments'
  surviving data), and **epoch residue** (a lower-epoch frame ends replay
  — discarded-life residue never re-enters a prefix;
  `RecoverStats::epoch_residue_stops`).
- Epoch derivation: every recovery-for-append resumes at 1 + max(epoch
  over the valid prefix and every validating beyond-frame); fresh logs
  start at 1. Carried on `SegmentRotor::{set_,}resume_epoch`; the cell
  assembly wires `StagingRing::set_frame_epoch` (seq restarts at 1).
- Writer surface: `StagingRing::seal(first_record_lsn, covered_lsn)` —
  the plane passes `GroupCommit::watermark()`; the synchronous tiers
  (`flush_into`) stamp `covered_lsn = 0` (attests nothing, conservative).
- v1 frames decode with `stamp() == None`, attest nothing, and keep the
  conservative pre-ADR-0031 refusal beyond a gap; a v1 frame *after* a v2
  frame in a replay prefix is a named refusal (append order — no
  downgrade after the first v2 frame).

## Batch frame layout v3 (`inf-log::frame`, M4.5-S34 — ADR-0086 D3)

Byte-identical to v2 from offset 4 on; the magic is `"IFR3"` and the
**successor rule** changes: the next frame begins at
`align_up(base + frame_len, FRAME_ALIGN = 4096)`, and the bytes between
`base + frame_len` and that boundary are zeros **written by the frame's
own write** (the `O_DIRECT` alignment unit — one aligned write-through
write per frame is the point). `frame_len` excludes the padding;
`FrameRef::padded_len()` is the reader's advance.

- Readers (`SegmentReader`, `FrameIter`, the tail scanner) round up after
  a v3 frame and apply the end-of-log rule at the boundary (zero magic ⇒
  preallocated tail; a torn frame stops the read). The padding bytes are
  **skipped, never validated** — a padding sector that did not land is
  not evidence about the frame (the CRC is), and demanding zeros would
  refuse honest torn tails on 512-byte-sector devices. `ReadEnd::at`
  is the aligned boundary — the resume offset.
- Which frames are v3: the writer emits v3 on `Direct` segments
  (`FrameLayout::Aligned`) and v2 on `Buffered` segments
  (`FrameLayout::Packed`); `FrameSlot::layout()` carries the choice from
  the rotor to `StagingRing::seal(first_record_lsn, covered_lsn, layout)`
  and `FrameBuilder::finalize(first_lsn, stamp, layout)`. v2 and v3 may
  interleave in either order (a mode change across lives); the "v1 after
  v2" refusal is unchanged.
- ADR-0031 D3's continuity rule "frame n+1 begins exactly where frame n
  ends" reads "at frame n's successor"; seq/epoch/attestation rules and
  the beyond-data-end audit are otherwise unchanged (the audit scans
  *from* the aligned data end, so a valid frame's padding is never
  mistaken for slack).
- Disclosure: `StagingStats::padding_bytes` (`log_padding_bytes` in
  INFO) counts every padding byte sealed; the watermark, `queued_up_to`,
  and the rotor's cursor advance by the padded length.
- The staging buffers carry `FRAME_ALIGN` slack on both ends and build
  the frame at an aligned base (`FrameBuilder::with_capacity`, the
  ADR-0054 D2 `align_offset` shape — no unsafe); `resident_bytes` =
  2 × (capacity + 8 KiB).

## `first_lsn` bound — post-M2-exit amendment (ADR-0126, 2026-08-17)

Post-exit change to a frozen surface, ADR-gated per the freeze discipline.
The nightly `frame_decode` campaign found that `first_lsn.offset` was read
from the header and never bounded: a CRC-valid frame declaring
`offset = u32::MAX` made `RecordIter` advance the record cursor past the
`u32` ceiling and panic in `Lsn::advance`.

- **The derivation is per-version.** `first record = frame base +
  header_len` — 20 for v1, 40 for v2. ADR-0011 D2's text says "frame base
  + 20", which is v1-era wording from before ADR-0031's 40-byte v2 header;
  ADR-0126 D1 restates it. **Cite `header_len`, never the constant 20.**
- **`decode_frame` bounds the field** where it is read, for both formats:
  `header_len ≤ first_lsn.offset` and `(first_lsn.offset − header_len) +
  extent ≤ u32::MAX`, where `extent` is the frame's **on-device extent**
  — `frame_len` for v1/v2, `align_up(frame_len)` for v3 (ADR-0126 D1 as
  amended 2026-09-14: the successor address and every segment cursor
  advance by the padded extent, so that is what must fit).
  A v3 `frame_len` that cannot be padded inside a `u32` is `BadLength`;
  `align_up_frame` itself saturates at `u32::MAX` instead of wrapping to
  0. A frame's first record is never inside its own header, and a frame
  always fits inside a u32-addressed segment.
- **New public variant `FrameDecodeError::BadFirstLsn { offset: u32 }`** —
  corruption class, not the torn-tail class (`ZeroMagic` stays the only
  expected end-of-log signal). `FrameDecodeError` deliberately stays
  exhaustive (no `#[non_exhaustive]`, ADR-0126 D3): the added variant is
  semver-breaking for external matchers, accepted pre-1.0 and revisited at
  1.0. Decoder error enums grow by ADR, each naming its semver impact.
- The bound also makes `inf-server::recover`'s `first_lsn.offset −
  header_len` frame-base subtraction sound on its own, instead of relying
  on `SegmentReader::next_frame`'s physical-offset cross-check having run
  first.

## LSN addressing (`inf-log::lsn`)

`Lsn = { segment: SegmentId(u32), offset: u32 }`, per cell; ordering is
(segment, offset) lexicographic = append order. A record's LSN is the byte
offset of its length prefix within its segment. **No global LSN exists**:
each cell's log is ordered on its own, and nothing orders two cells' logs.

`inf-log::MAX_SEGMENT_LEN` is the largest physical segment length whose
one-past end is a representable LSN offset, and `check_segment_len` is the one
refusal (ADR-0018, 2026-09-08 amendment). Recovery applies it to every
retained segment before loading a checkpoint or replaying records; both
tail-scan entry points and `SegmentReader::open` apply it to the opened file
before reading. Every cursor advance and frame-implied address in both read
paths is checked, so `new` — which takes a caller's handle — cannot wrap onto
a live frame's address either. A refusal is `io::ErrorKind::InvalidData`
naming the segment, its length and the limit.

The bound is the format's address range, not the current rotation size: an
older segment may exceed today's `segment_bytes` and stay replayable. The
rotor seals before a frame would cross `segment_bytes` (a `u32`), so an
oversized file is planted or foreign. Refusal never truncates the file or
ignores its unaddressable suffix.

## Segment naming + lifecycle (`inf-log::segment`, `inf-log::scan`)

- Files: `seg-{:06}.ilog` under `shard-k/log` (ids > 999999 grow digits);
  parser accepts 6–10 digits ≤ `u32::MAX`. Non-canonical padding parses so
  the boot scan reports it as a **Duplicate** id rather than skipping.
- Boot scan: contiguous ascending sequence required (may start > 0 after
  truncation); anomalies are named errors — `BadName` (foreign/truncated
  names), `Duplicate`, `Gap` — never silent skips.
- Lifecycle: next segment preallocated in MAINTAIN
  (`SegmentRotor::maintain`); rotation on the append path is a pointer
  swap; seal = fdatasync + write-handle drop (sealed segments are
  immutable by construction). Seal at `segment_bytes` (default 256 MiB)
  or optional `seal_after_ms` (default off — M2 cut line; **synchronous
  tier only**: `maintain_deferred` never time-seals — the synchronous
  `rotate()` would swap the active segment under an in-flight frame).
- **Segment recycling (M4.5-S39b, ADR-0090):** a sealed `Direct` segment
  that was pre-zeroed (`SealedMeta { id, prezeroed }`, recorded at seal)
  and falls below the MANIFEST floor is offered to a bounded per-cell
  pool at truncation instead of being unlinked —
  `SegmentRotor::forget_sealed(id) -> SealedDisposal::{Recycled,
  Unlink(PathBuf)}`, bound `SegmentConfig::recycle_slots` (**default 1**
  — ADR-0090 A7.2; `0` / `--no-segment-recycle` is the baseline arm;
  campaign F's recovery gate was non-discriminating and its attribution
  is S39d's). The MAINTAIN prealloc consults the pool before creating a
  file: `rename(seg-M → seg-N)`,
  open `N` direct, `fully_allocated()` **re-read** (ADR-0086 D4 — a
  pooled file that reads sparse falls through to the zero-fill,
  `recycle_fallbacks`), and on the deferred tier the rename's directory
  entry rides the **same `PreallocBarrier`** a fresh prealloc's does —
  registered coverage-neutral in the ledger in the slice that renamed,
  so every write-through ticket of `N`'s frames sits behind it and no
  record of `N` is acknowledged before the rename is durable (ADR-0090
  D3 as amended; pinned in `commit.rs`, the rotor tests, `m2-recycle`).
  Counters: `segments_recycled`, `recycle_misses` (prealloc found the
  pool empty), `recycle_fallbacks`, `recycle_pool_bytes`. Ids are never
  reissued: `N = active.next()` — a recycled file's residue is stamped
  with every previous id it carried, all ≠ `N`.
  **The take-time sentinel (ADR-0090 A15):** every take
  of a fully allocated pooled file writes one v3 frame stamped for the
  file's **old** id into its **last block** (`build_recycle_sentinel(
  &mut FrameBuilder, old, segment_bytes)`, record `Delete { ns: 0, key:
  RECYCLE_SENTINEL_KEY }`, stamp `{1, 1, 0}`) — foreign under `N`, so the
  slack scanner always meets one foreign frame and the residue stays
  provable when the next life's data end lands inside the last residue
  frame's body (the one placement no surviving header proved). It rides
  the zero-fill state machine as a one-block `Filling` with
  `FillSource::RecycleSentinel { old }` (unpaced; ready after the fill
  barrier — durable before the file can be taken write-through) on the
  driver tier, and is written + `sync_data`ed inline on the synchronous
  tier. `zero_fill_bytes` counts zeros only; `recycle_sentinels` counts
  sentinels (INFO `recycle_sentinels`).
- **The pool wait (ADR-0090 D9 / A8):** `SegmentConfig::prealloc =
  PreallocPolicy::{Immediate, WaitForPool { bound: Quarter | Eighth }}`
  (`infinityd --recycle-wait off|quarter|eighth`, default `quarter`).
  The rotor keeps one explicit state while `next` is absent, reset to
  `Immediate` at every rotation: `Immediate → WaitingForRecycle` when the
  pool is empty and the wait is eligible (the rotor is `Direct`,
  recycling on, the active segment pre-zeroed, ≥ 1 rotation this life, a
  sealed pre-zeroed segment of this life exists, size-based sealing,
  `active.written < bound`); each MAINTAIN slice re-checks the pool;
  `→ FreshFallback { fill_origin }` at the bound (one `recycle_miss` per
  generation, never per slice; a `NoSpace` retry neither waits nor counts
  again). The fallback's zero-fill paces from `fill_origin` (`allowed =
  2 × (written − origin) + 16 MiB`) so the head start stays ADR-0086
  D4's burst and the fill completes by ¾ − 8 MiB; an immediate prealloc
  has origin 0 and is byte-identical to the pre-D9 pacing. ENOSPC
  surfaces at the bound with ¾ of the segment as admission headroom.
  Counters: `recycle_waits_started` / `_satisfied` / `_expired`,
  `recycle_wait_active_bytes_max`, `segment_inline_preallocs`,
  `segment_prealloc_failures`. Invariants (pinned in
  `segment_prealloc_wait.rs`, the `m2-recycle` oracles): every wait ends
  exactly once; a waiting policy never strands a rotation
  (`inline_preallocs = 0`); a wait nothing can feed is never started.
- ENOSPC discipline: prealloc failure raises `space_exhausted()` *before*
  writes need the space (the S08 admission hook); on `MemFs` an append that
  outruns it gets typed `LogError::NoSpace`; on the shipped backend a WAL
  write that fails, `ENOSPC` included, ends in the cell's fail-stop
  (`DurableCell::on_log_error`). fsync failure is the distinct,
  non-recoverable `FsyncFailed` type (the fsyncgate rule — enforced
  since S17 by `scripts/check-fsync-fail-stop.sh`: the fsync-error types
  may appear only in the audited allowlist of fail-stop sites; on the
  node, `DurableCell::fail_stop` exits with
  `inf_server::EXIT_DURABLE_FAILSTOP` (= 3) after freezing the watermark
  — zero acks for the affected batch, proven by the child-process
  fsyncgate test).
- Append protocol: `begin_frame(len, now) -> FrameSlot` (rotates if
  needed, reserves the base LSN) →
  `FrameBuilder::finalize(slot.first_record_lsn())` → `commit_frame(slot,
  bytes) -> Lsn`.

## `SegmentFs` injection seam (`inf-log::fs`)

Control-path file operations (create/prealloc, dir-fsync, list, open,
positional read/write, fdatasync) behind a trait so DST can fault every
one (L7). Tiers: `StdSegmentFs` (boot/dev; `set_len` prealloc, which
reserves no blocks: no tier calls `fallocate`), `fs::mem::MemFs`
(deterministic, fault-injectable test tier — process-KILL physics: every
completed write survives), `fs::sim::SimDisk` (M2-S18 — power-CUT
physics: un-fsynced state loses/tears/reorders, see the sim-disk section
below). The per-iteration hot-path write + linked fsync is **not** part
of this seam — it rides `BackendDriver` (S05); the sim tier executes
those driver ops against the same `SimDisk`.

### S08 additions (ADR-0015 D3)

- `SegmentFs` gained `rename` + `remove_file` — the atomic small-file swap
  vocabulary (`inf-log::meta`: write `META.new` + fsync + rename +
  dir-fsync, CRC32C-enveloped, payload-opaque). First consumer: the
  namespace catalog, single-written by `inf-server::control`; S11's
  MANIFEST reuses the protocol.

### M4.5-S34 additions (ADR-0086 D4) — `Direct` log segments

> **ADR-0191 (2026-10-29), implementation open:** `SegmentConfig::assert_valid`
> goes. `Direct` still requires `segment_bytes % 4096 == 0`; the check is row 4
> of `LogGeometry::new`, the one constructor of the log's checked pair (the
> log staging domain section, below), which also requires
> `segment_bytes ≥ max_sealed_frame_bytes(staging, io_mode)`: a segment holds
> at least the largest frame the staging ring can seal as it lies on the
> device — `capacity_bytes` packed on a `Buffered` segment, `align_up(
> capacity_bytes, 4096)` on a `Direct` one — and under `Direct` the two rows
> together read `segment_bytes ≥ capacity_bytes`. `SegmentRotor::create_fresh`,
> `create_fresh_deferred` and `open_existing` take a `LogGeometry` in place of a
> `SegmentConfig` and keep `geometry.segment()` as their configuration; no
> other constructor takes a `SegmentConfig`. A refused pair is a
> `LogGeometryError` at construction, never a panic and never a frame refused
> while serving: `LogError::FrameTooLarge` goes, and `begin_frame` and
> `begin_frame_deferred` keep the compare
> `padded_bound(frame_len) ≤ segment_bytes` as a release assertion over an
> invariant the geometry proves.
> `SegmentIoMode::layout()` names the mode's `FrameLayout` once
> (`Buffered → Packed`, `Direct → Aligned`). Not built; the bullets below
> state the shape as implemented today (ADR-0191 D1, D3, D4).

- `SegmentIoMode::{Buffered, Direct}` on `SegmentConfig::io_mode`
  (default `Buffered` — the M2 path byte-for-byte) plus
  `fua_max_frame_bytes` (the probed write-through crossover, default
  256 KiB). `Direct` requires `segment_bytes % 4096 == 0`
  (`SegmentConfig::assert_valid`).
- `SegmentFs::create_segment_direct(path, prealloc)` — `O_DIRECT`
  (verified), sparse, unsynced; `SegmentFs::open_segment_append(path,
  mode)` — the recovered tail reopens in the configured mode;
  `SegmentFile::fully_allocated()` — the **read, never remembered**
  pre-zeroing fact (`st_blocks × 512 ≥ st_size` on the std tier; `true`
  on `MemFs`; length ≥ preallocation target on the sim disk). Wrappers
  (`ReadAheadFs`) forward explicitly, as for `create_tier`.
- Rotor zero-fill state machine (driver-ridden, never a blocking write):
  `next_zero_slice(max_len) → ZeroSlice{fd, offset, len, source:
  FillSource::{Zeros, RecycleSentinel { old }}}` (paced against
  `2 × active.written + ZERO_FILL_HEAD_START` while the active segment
  is pre-zeroed and the source is zeros; unpaced otherwise — the plane
  copies a sentinel image into its own aligned one-block window before
  the push), `note_zero_slice_written()`,
  `take_zero_fill_barrier() → fd`, `note_zero_fill_synced()`;
  `FrameSlot::{len (padded), layout, write_through_ok}`;
  `LogError::NextNotReady` (a zero-fill op is in flight on the segment
  rotation needs — the frame waits one completion); class-upgrade
  rotation at `begin_frame_deferred` when a pre-zeroed next segment is
  ready and the active one is not; `RotorStats::{zero_fill_bytes,
  rotations_unzeroed, rotations_upgrade}`;
  `active_io_mode()`/`active_write_through()`.
  **Amended 2026-09-30 (ADR-0167 D2):**
  `ZeroSlice.offset` is `u32`, the segment cursor it is built from; its
  `LogWrite` position is `FileOffset::from_u32_bytes`, total.
- Sim disk: `create_segment_direct` creates an empty inode with a
  preallocation target; `driver_write_through(fd, offset, data)` is
  durable at completion and supersedes overlapping earlier pending
  writes; `StallConfig::through_base_ns` (0 = inline) and
  `schedule_write_through(now)`.
  **Amended 2026-09-30 (ADR-0167 D2):**
  `SimDisk::driver_write_at`, `driver_write_through` and `driver_read_at`
  take `offset: FileOffset`.

### M4-S09/S11 additions (ADR-0054, ADR-0056)

- `SegmentFs::create_tier(path, mode)` (M4-S09, ADR-0054 D1) and
  `SegmentFs::open_tier(path, mode)` (M4-S11, ADR-0056 D5) — tier files
  carry a per-file I/O mode ({`Buffered`, `Direct`}, fixed at open,
  verified on `StdSegmentFs`, never a silent fallback). Defaults
  delegate to `create_segment`/`open_write` (mode-equivalent on the
  in-memory/sim tiers); **wrappers must forward both explicitly**
  (`inf-server::ReadAheadFs` does — falling into the default drops the
  flag).
- `SegmentFile::truncate(len)` (M4-S11, ADR-0056 D5) — `ftruncate`
  semantics; sole caller is the tier recovery pre-flush rule
  (un-manifested tier bytes are dead-life garbage). Durable only after
  a following `sync_data`; the sim tier models the un-synced window
  honestly (a power cut may resurrect the old tail).

### Namespace residue additions (ADR-0189 D8)

> **ADR-0189 (2026-10-10):** the seam gains the two calls the run-time
> residue sweep of a dropped durable namespace needs.
> `list_dir_bounded(dir, names_max, bytes_max) -> io::Result<Listing>`,
> `Listing::{Absent, Names(NameBatch)}`, returns at most `names_max` names
> and `bytes_max` name bytes, in any order, each name the entry's own bytes
> (`OsStr`), never converted lossily as `list_dir` converts them.
> `remove_dir(dir) -> io::Result<DirRemoval>`, `DirRemoval::{Removed,
> Absent}`, answers any other errno as an `Err`. On the simulated disk,
> `remove_dir(d)` answers `ENOTEMPTY` while a name or a directory of the OS
> view has parent `d`; otherwise `d` leaves the directory set at once, as a
> pending `MetaOp::RemoveDir` that the parent's `sync_dir` commits to a
> `durable_dirs` twin. Applying it, in a commit or in a cut's surviving
> prefix, first applies `d`'s own pending operations in issue order; a cut
> restores the directory set from `durable_dirs`. `create_dir_all` inserts
> into both sets, so a directory is durable at creation; the residue sweep
> creates none, since a namespace id is never reused. After each commit and
> each cut the disk asserts that no durable name lies under an absent
> durable directory. The simulator's restart reuses the node's disk, so a
> restart without a power cut keeps every pending `MetaOp`. The fault
> points `ns_dir_remove_fail` and `ns_dir_sync_fail` fire inside those two
> calls, and the disk logs every call. Not built.

## `MutationEffect` → record seam (`inf-log::effect`, ADR-0012)

```rust
enum MutationEffect<'a> {
    StringSet { ns: NsId, key: &'a [u8], value: &'a [u8] },  // → tag 1 post-image
    Delete    { ns: NsId, key: &'a [u8] },                   // → tag 2
    ExpireAt  { ns: NsId, at_unix_ms: u64, key: &'a [u8] },  // → tag 3 (absolute)
    NsOp      { ns: NsId, payload: &'a [u8] },               // → tag 4
    CkptBegin { ckpt_id: u64 },                               // → tag 5, ns 0
    DocDelta  { ns: NsId, key: &'a [u8], lineage: DocLineage,
                base_version: u32, match_count: u32, post_len: u32,
                opcode: u8, program: &'a [u8], operand: &'a [u8] }, // → tag 6
    DocFull   { ns: NsId, key: &'a [u8], lineage: DocLineage,
                version: u32, idoc: &'a [u8] },              // → tag 7
}
```

- `record() -> RecordView<'a>` is the encoder registry. Document variants
  are borrowed field views and add no composed payload buffer or
  document-aware frame branch. `encoded_len()` is exact (admission +
  accounting input).
- Defined in `inf-log` (the seam's consumer); `inf-store` imports
  it when S08 wires durable namespaces (the dep-DAG edge lands there,
  direction fixed by ADR-0012).
- `DocFull` replay is a blind idempotent upsert after validating canonical
  idoc bytes and installs its exact nonzero lineage plus u24 version.
  `DocDelta` validates the path program and opcode operand, then orders by
  lineage before version: an absent/expired key counts
  `SkippedDocDeltaMissing`; a non-document key or document with newer
  lineage counts `SkippedDocDeltaStale`; an older document lineage is
  corruption. Equal lineage uses
  `distance = (current - base) mod 2^24`: zero applies through the same
  `inf-doc::apply` semantics and bumps once; `0 < distance < 2^23` is a
  counted stale skip; the other half-range is corruption. Replay uses the
  record's exact `match_count` and `post_len`, not current boot limits,
  and fail-stops if the result count or canonical output length disagrees,
  or if re-executing the delta crosses the format bounds every writer
  enforces: nesting past 128 containers or a document past the record clamp
  (ADR-0169 D6; a delta a pre-fix build wrote that way is corruption,
  ADR-0042 A1).
  This is ADR-0043 D6, including recreation and version
  wrap.

## Log staging domain (`inf-log::staging`, M2-S03)

The "log staging ring": a double-buffered frame pair (contiguity for
one-writev — ADR-0012), fixed capacity, allocated once per cell.

**M4.5-S35 amendment (ADR-0087 D1):** the pair is a **ring of
`frames_in_flight + 1` whole frame buffers** (`StagingConfig { capacity_bytes,
frames_in_flight: 1..=MAX_FRAMES_IN_FLIGHT (8) }`, default 1 — the pair,
byte-identical). Up to K sealed frames stay leased at once; `release(lease)`
frees its buffer by generation in **any order** (completions reorder);
`seal` takes any free buffer; `can_seal()` = records pending ∧ a free buffer;
`backlogged()` = every in-flight slot taken; `drained()` = no lease out (the
rotation precondition, ADR-0087 D4); `in_flight()`, `frames_in_flight()`.
`capacity_bytes` keeps its per-buffer meaning (so `max_record_len`, a
user-visible admission contract, does not move with K); `resident_bytes` =
(K + 1) × (capacity + 2 × `FRAME_ALIGN`). `StagingStats::in_flight_max` is
the observed high-water mark (`frames_in_flight_max`). Operator surface:
`infinityd --frames-in-flight K`; the L5-neutral pairing is `--frames-in-flight
3 --log-staging-mib 2` (4 × 2 MiB). `StagingConfig::with_capacity(bytes)`
is the K = 1 constructor tests and tooling use.

> **ADR-0191 (2026-10-29), implementation open:** a ring is built from a
> `LogGeometry`, the log's checked pair of a `StagingConfig` and a
> `SegmentConfig`: `StagingRing::new(geometry: LogGeometry)` reads
> `geometry.staging()`, and no other constructor takes a `StagingConfig`.
> `LogGeometry::new(staging, segment) -> Result<LogGeometry, LogGeometryError>`
> is the type's only constructor. It refuses, as a typed error naming the
> numbers, a `capacity_bytes` below `MIN_FRAME_LEN` (48) or above
> `DEFAULT_MAX_FRAME_LEN` (64 MiB), a `frames_in_flight` outside
> `1..=MAX_FRAMES_IN_FLIGHT` (8), a `Direct` `segment_bytes` that is not a
> multiple of `FRAME_ALIGN`, and a `segment_bytes` below the largest frame the
> ring can seal as it lies on the device, `max_sealed_frame_bytes(staging,
> io_mode)` — `capacity_bytes` on a `Buffered` segment (packed),
> `align_up(capacity_bytes, 4096)` on a `Direct` one (aligned); nothing is
> allocated before the check. `LogGeometry::DEFAULT` is `new` of the two
> defaults, evaluated at compile time; `with_staging(staging)` is `new` with
> the default segment, for a ring with no rotor. `StagingConfig`,
> `with_capacity(bytes)`, `capacity_bytes`'s per-buffer meaning,
> `max_record_len` and `resident_bytes` are unchanged: the geometry adds a
> check, not a value. `infinityd` builds the geometry once the device probe
> has named the class and refuses a pair the type refuses before it listens,
> naming `--segment-bytes` and `--log-staging-mib`; `--log-staging-mib`'s own
> domain is `1..=DEFAULT_MAX_FRAME_LEN >> 20`, written from the constant. Not
> built; the bullets below state the shape as implemented today (ADR-0191
> D1, D3).

- EXECUTE: `stage(&MutationEffect) -> Result<StagedAt, StagingFull>` —
  in-place encode, zero steady-state allocation (counting-allocator
  enforced); `StagingFull{needed, available}` is the typed backpressure
  that stops durable read-rearm (wired S05/S08); `would_fit`/`backlogged`
  are the admission predicates.
- LOG: `can_seal()` → `seal(first_record_lsn) -> FrameLease` (staging
  swaps to the free buffer; at most one frame in flight) →
  `leased_frame(&lease)` for the writev → `release(lease)` on completion.
  `flush_into(&mut SegmentRotor, now_ms)` is the synchronous pre-S05
  choreography; on a failed write it releases the lease and returns the
  error — the failed frame's records are gone with the buffer, the ring
  is never wedged.
- LSN handoff (L6): `FrameLease::lsn_of(StagedAt) -> Lsn`,
  generation-checked — the S06 `WatermarkGate` registration input.
- Accounting (L5): `staged_bytes`/`in_flight_bytes` exact at every
  append/seal/release; `resident_bytes` = 2 × capacity, constant;
  `capacity_bytes()` (M4.5-S27 — the `log_staging_capacity_bytes`
  observable, operator-set via `infinityd --log-staging-mib`);
  cumulative `StagingStats{appends, append_bytes, refusals, seals,
  releases}`.
- M4.5-S27 amendment (ADR-0083 D1/D2): admission behaviour above this
  seam changed — every parkable path (local pump *and* the fabric
  per-origin pump) paces on `StagingFull` instead of refusing; typed
  `-BUSY` is no longer the design response to pressure. A record that
  can never fit (`est > max_record_len`) refuses up front with a typed
  `ERR` (the M2-S08 up-front bound check this section always demanded).

## Sequential read path (`inf-log::reader`, M2-S04)

`SegmentReader` over one segment (sealed or active tail), reads through
the `SegmentFs` seam (→ `BackendDriver` at S05):

- `next_step() -> Result<ReadStep<'_>, ReadError>` is the lending,
  validate-then-yield operation (one CRC pass per frame via header peek).
  `ReadStep<'a> = Frame(FrameRef<'a>) | End(ReadEnd)` carries the clean
  end in the return value; no caller must infer it from an absent frame.
  A clean end repeats on subsequent calls. An invalid frame remains at
  the failing cursor and subsequent steps report its error again; read
  errors never authorize skipping bytes or treating failure as clean EOF.
- `next_frame() -> Result<Option<FrameRef<'_>>, ReadError>` is the
  compatibility wrapper: `Frame` becomes `Some`, `End` becomes `None`.
  After a terminal read failure it fuses to `Ok(None)`; this is **not**
  a clean end (`read_end()` remains `None`). `read_end()` returns `Some`
  only once either stepping API has recorded a clean `ReadEnd`.
- `apply_frames(callback) -> Result<ReadEnd, ApplyError<E>>` loops over
  `next_step`: apply each validated frame, return its explicit clean end,
  propagate `ApplyError::Read` on read failure. A callback failure returns
  `ApplyError::Apply { at: frame.first_lsn(), error }`; that frame has
  already been consumed. A `FrameRef` borrows the reader window and must
  be released before another step.
- Stored `first_lsn` cross-checked against physical offset per frame —
  `ReadError::LsnMismatch` on misdirected writes (ADR-0011 D2). **M4.5-S39b
  (ADR-0090 D2 as amended):** a frame that decodes in full (magic, length,
  record count, CRC32C) whose stored **offset equals** the physical offset
  while its stored **segment id differs** from the file's is the typed
  `ReadError::ForeignSegment { segment, offset, stored_segment }` — the
  residue a recycled segment carries from its previous life. Both
  variants are terminal for the reader: it never yields, skips or
  truncates a misplaced frame (the invariant every consumer relies on is
  unchanged); only recovery classifies the distinction.
- Tail = facts, not policy (S14 owns the taxonomy):
  `ReadEnd::{ZeroTail, FileEnd}` with `.at()` = byte after the last valid
  frame (the `SegmentRotor::open_existing` tail_offset); torn/corrupt
  bytes are typed `ReadError::Frame{segment, offset, error}` — the reader
  never truncates and never skips.
- `ReaderConfig{chunk_bytes = 1 MiB, max_frame_len}`; window grows only
  when a frame exceeds it (bounded by `max_frame_len`).

These are reader facts; recovery decides from them. A validating frame
beyond a segment's data end marks a hole (ADR-0087 D6). Recovery refuses
to start, with a `LogCorruption` naming the corruption point and the
evidence, when a frame beyond the hole is format v1 or a surviving frame
attests fsync coverage past it; otherwise it truncates at the hole and
counts the frames it discards. Frames whose epochs all sit below the
replayed prefix's or below a life observed beyond the hole, or whose own
`first_lsn.segment` is not the segment they were read from, are residue
of a discarded or recycled life, never a hole; when the epoch beyond the
hole decides it, the segments probed beyond the hole are replayed.
Non-validating residue at the resume point is a torn tail (the tail
pointer moves back; no byte is rewritten); behind it, residue is
tolerated and counted. A log that ends below the MANIFEST's begin-LSN
refuses to start (ADR-0018 D2/D5, ADR-0031 D4/D5, ADR-0090 D2).

## Driver file ops (`inf-runtime`, M2-S05, ADR-0013 D1)

Extension of the frozen M0 `BackendDriver` contract (recorded per the
interface-freeze discipline). The token *layout* is unchanged; `TokenClass` gains
`LogWrite = 5` and `Fsync = 6`.

- `IoOp::LogWrite { fd, offset, data: StableBytes, token, barrier:
  WriteBarrier }` — positional write of one sealed frame (the contiguous
  frame makes "one writev" a single-iovec write). Short writes resubmit
  internally: `CompletionResult::LogWritten` ⇒ ALL bytes reached the fd.
  **Amended 2026-08-21 (M4.5-S34, ADR-0086 D1):** the former
  `fsync_token: Option<CompletionToken>` became the `WriteBarrier` enum —
  `None` (page cache / device cache, the staging-lease release point,
  never an ack point), `WriteThrough` (`RWF_DSYNC` on an `O_DIRECT` fd:
  `LogWritten` is delivered only once the bytes are on stable media — the
  frame's durability fact, no `Synced` follows; a short write re-arms its
  remainder write-through), `LinkedFsync { fsync_token }` (below). The
  state "write-through and a linked sync" is unrepresentable.
  **Amended 2026-09-30 (ADR-0167 D2):** `offset` is
  `FileOffset`. A log frame's and a zero fill's position come from a
  `u32` segment cursor and a checkpoint header's is 0 (all total); a
  checkpoint section's or footer's is checked before `admit`, and a
  refusal aborts the checkpoint with nothing admitted or sealed. A tier
  flush round's write positions are checked when the round opens (the
  flush-round inventory below).

  > **Accepted 2026-10-19, implementation open — ADR-0179:** a `LogWrite`,
  > `Fdatasync` or `TierRead` whose class (log, tier flush or tier read)
  > is full waits in push order for the first driver entry with room; it
  > is never refused and its completion is unchanged. A short write with a
  > linked sync issues its remainder only once the superseded sync's
  > completion is reaped, so one frame holds one op slot.

- `WriteBarrier::LinkedFsync { fsync_token }` chains an fdatasync —
  `IOSQE_IO_LINK` on uring (kept unsplittable across submit boundaries),
  issued after the write's completion on fallback tiers.
  `CompletionResult::Synced` is the durability fact for this class (L2)
  and is delivered exactly once, only after every byte of the write is
  both written and covered (a sync that raced a short write is superseded
  internally); a failed write cancels it (`Error{ECANCELED}` on
  `fsync_token` — no sync-past-failed-write). kqueue executes
  `WriteThrough` as write + fsync (the correctness tier).
- `TokenClass::ZeroFillWrite = 13` (routing-only, ADR-0086 D4): a plain
  `LogWrite` zero-filling a preallocated next segment; its completion
  advances `SegmentRotor`'s zero cursor, never the staging lease.
- `IoOp::Fdatasync { fd, token }` — standalone barrier (everysec tick,
  deferred seal).
- `StableBytes::new(&[u8])` is the one `unsafe` seam: bytes must stay
  live/stable/unmodified until the op's terminal completion — the staging
  `FrameLease` is the canonical proof (buffers never reallocate; reset only
  on release). Construction lives in the plane; `inf-log` stays
  `#![forbid(unsafe_code)]`. S08 decides `inf-server`'s shape (ADR-0013).
- `IoOp` has no `fallocate` or rename op and no op of its own for a
  directory sync: a rename is a `SegmentFs` call; a directory sync is a
  blocking `SegmentFs::sync_dir` or an `IoOp::Fdatasync` on a directory fd
  (the boot barriers, the checkpoint directory's sync); no production path
  reserves blocks with `fallocate` (write-through segments are pre-zeroed
  instead, ADR-0086 D4).

### Amendment (2026-08-19, M4.5-S31 — ADR-0084): tier-flush token classes

The token *layout* stays frozen; `TokenClass` gains two routing-only
classes: `TierFlushWrite = 11` and `TierFlushSync = 12`. The ops are the
existing `IoOp::LogWrite`/`Fdatasync` (the ADR-0056 alternatives-rejected
rule) — only the completion routing differs: these classes route to the
tier plane's round bookkeeping (`TierCell::on_flush_completion`), never
to `DurableCell` (the frame-lease custody chain is not theirs) and never
into the WAL commit ledger (flush watermarks are a separate custody
chain; acks never wait on a tier barrier — ADR-0022 D3 untouched). Token
payload: `slot = lane × 256 + op_index` (lane = the namespace's stable
per-cell flush lane; ≤ 256 ops per round, asserted), `generation =
round_seq` (stale completions mismatch and are counted, never applied).
A third `StableBytes` construction (`log_bytes::tier_round_bytes`) joins
the audit surface with the round-custody proof (SAFETY.md).

## M2.5-S01/S07 amendments (boot barriers · sync pipeline — ADR-0026)

Post-M2-exit changes to frozen surfaces, ADR-gated per the freeze
discipline:

- **`SegmentFile::advise_read_ahead(offset, len)`** (default method,
  no-op — M2.5-S08, ADR-0028): a hint that the caller will read
  `[offset, offset + len)` sequentially next. `SegmentReader::refill`
  hints the next two windows; `IckReader::next_step` hints per section.
  Hint-only by contract: implementations must not change any byte the
  reader sees (L7 — MemFs/sim keep the no-op). The real implementation is
  `inf-server`'s crate-private `ReadAheadFs` — a boot-scoped prefetch
  thread held only by `Recovery::boot_reads` (ADR-0109: the plane, the
  rotor and the tier handles keep the bare tier by type); enabled by
  `RecoverConfig::boot_prefetch`, which the assembly sets iff a single
  cell recovers — the measured regime split.
  Also under this ADR: `read_ick_counts` gained a direct end-of-file
  footer probe (CRC-validated, hops as fallback) — a parse-path change
  covered by the extended `ick_decode` fuzz oracle. ADR-0028 A1
  (2026-09-12): the probe locates the footer by the
  footer's **own** namespace count (an empty durable namespace is absent
  from it), and `read_ick_counts_probed` reports whether it hit.
- **`SegmentFs::create_segment_unsynced`** (default method, falls back to
  the synced create): segment creation with no durability side effects —
  the caller owns metadata durability via ledger barriers. Consumers:
  `SegmentRotor::create_fresh_deferred` (boot) and
  `SegmentRotor::maintain_deferred` (next-segment prealloc), both reactor
  tiers only; the synchronous tier (`open_cell_log`, tests, tooling)
  keeps blocking creates + dir-fsyncs.
- **Boot/prealloc metadata barriers** (`GroupCommit::register_boot_barrier`
  / `register_prealloc_barrier`, `SyncReason::{BootBarrier,
  PreallocBarrier}`): driver-ridden fdatasyncs on dir handles (held by the
  ledger until `Synced`) and the active segment fd. Boot barriers enter at
  the head of the ledger covering the recovery floor; prealloc barriers
  enter **coverage-neutral** at the tail. The done-prefix rule fences
  every durable ack (and manifest publication) behind them, so these
  barriers do not hold boot-ready; the checkpoint probe does:
  `CkptCell::new` runs `probe_direct`, one direct write and one fdatasync
  on the cell thread during recovery, before the cell is ready. A blocking
  metadata fsync on a reactor thread is the ADR-0022 D7 wedge mechanism;
  the boot-storm DST scenario (`inf-sim --scenario boot-storm`) counts
  blocking `sync_dir` calls on the ready path and fails on any, and
  `inf-bench boot-storm` is the device-tier spawn-storm regression.
- **Recovery machine**: `Recovery::deferred_boot_sync()` (loop-resident
  boots), `take_boot_barrier_dirs()`, `phase_code()`; the RecoveryBoard
  slot gains a `phase` published **before** each step plus assembly
  setup-phase codes (10+) so a stalled step or setup syscall names itself
  in the control thread's stuck-cell narration.
- **Sync pipeline (M2.5-S07)**: `GroupCommit::with_sync_pipeline(1|2)` —
  1 = the ADR-0022 D3 one-in-flight discipline (default); 2 = the bounded
  two-in-flight pipeline (`DurableConfig::sync_pipeline`, `infinityd
  --sync-pipeline`). At bound 2 a completion CQE may issue the deferred
  sync immediately (`completion_fsync_due`/`register_completion_fsync`,
  `SyncReason::Completion`) while one slot stays reserved for the LOG
  step's linked sync; at bound 1 the completion path never issues.
  `register_standalone_fsync` now clears the always due exactly when the
  covered range discharges it (previously it was kept whenever always
  traffic was pending). **Retired 2026-08-21 (M4.5-S35, ADR-0087 D5):**
  the constructor is `GroupCommit::with_flush_bound(1|2)` and the field
  `DurableConfig::flush_bound` (FLUSH-class barriers in flight — the
  ADR-0022 D3 constant, 1 shipped); `infinityd --sync-pipeline` is refused
  with a pointer to `--frames-in-flight`; `inf-bench` accepts
  `sync-pipeline` for one campaign as a logged no-op and forwards
  `frames-in-flight` / `barrier-class` / `staging-mib` instead. The
  two-in-flight arm stays reachable by construction (tests, harness).
- **Formation observables**: `CommitStats` gains `fsyncs_completion`,
  `fsyncs_boot_barrier`, `fsyncs_prealloc_barrier`; `DurableStats`/`INFO
  persistence` gain `fsync_group_p50`/`fsync_group_p99` (records newly
  covered per durability-fsync completion — the M2.5 formation gate
  observable); `gate-run m2` emits `tripwire:group_formation_x` and
  `tripwire:spawn_retries` (whether the latest spawn needed its retry, 0
  or 1; no gate row reads it).

## Group commit + durability watermark (`inf-log::commit`, M2-S05/S06)

> **ADR-0197 D3 (2026-10-30):** a frame's flight (its staging lease, the write's submit time and
> the barrier its write settles) is the frame queue's entry, and the plane keeps no table of
> frames in flight. `note_frame_queued` takes the sealed frame's lease and the submit time in
> place of the frame's length (`(end, len) -> FrameId` became `(end, lease, now) -> FrameId`), and
> `note_frame_written(FrameId)` returns `WrittenFrame { lease, submitted_at, barrier }`, handing
> the flight back at the frame's one `LogWritten`.
>
> **ADR-0197 D1–D2 (2026-10-30):** `frame_plan(write_through_ok, seal_ahead)`
> answers `FramePlan::Seal(SealPlan)` or `FramePlan::Wait(Hold)`, with
> `SealPlan::{Plain, WriteThrough, LinkedFsync}`: a sealed frame's plan cannot hold `Wait`, and
> every sealing path takes a `SealPlan`. `note_frame_queued(end, lease, plan, now) ->
> FrameQueued { id, barrier }` queues the frame with the barrier its plan chose: the barrier's
> ledger entry covers the frame's own end, and the queue entry is published once with its flight
> whole. `register_linked_fsync` and `register_write_through` go, so no barrier is registered
> apart from its frame and a frame takes its barrier once. A linked plan still needs `drained()`
> and a write-through plan the prefix rule (the frame's base is `queued_bytes`), each a release
> assertion in the queueing on a plan made in the same LOG step.

**The ledger's classes (2026-10-29).** Every `SyncReason` belongs to one
`LedgerClass`, and each class holds at most its budget of entries unfolded:
boot 5 (`BootDirs`' at most four directory handles and the fresh
segment's), the rotation's seal 1, prealloc barriers 2 and zero-fill
barriers 2, extent 16 (the extent-seal window is that class's count),
write-through 16 and discretionary (linked, standalone, completion) 2 —
`FSYNC_LEDGER_ENTRIES` = 44, the ledger's cap. A class is read before its
entry exists: `frame_plan` answers `Wait(Hold::SealUnfolded)` for a frame
that needs a rotation while the previous seal is unfolded (the seal holds
the old segment's file and covers its end, so it neither coalesces nor goes
plain), declines write-through and linked syncs at their budgets (`Plain`,
the due accumulating), `standalone_fsync_due` and `completion_fsync_due`
read the discretionary class, and MAINTAIN skips a prealloc or zero-fill
barrier at its budget. `class_unfolded(LedgerClass)` and
`class_has_room(LedgerClass)` are the reads; a registration past its class's
budget is a release fail-stop (1.26). `FramePlan::Wait` carries its reason,
`Hold::{ReorderWindow, Barrier, SealUnfolded}`, so the plane counts each in
its own counter (`frame_waits_reorder`, `frame_waits_barrier`,
`frame_waits_ledger`) without asking again. The frame queue, the ledger and
the plane's ack map are `CappedDeque`s of 16, 44 and 61 entries, rows
`frame-queue`, `fsync-ledger` and `ack-map` of the cell's census; the plan
asks the queue's own admission, and the LOG step asks the ack map's before
the plan (`frame_waits_ack_map`, derived never to occur).
`note_frame_queued(end, lease, plan, now) -> FrameQueued { id, barrier }`
takes the sealed frame's staging lease into the queue entry with the
barrier its `SealPlan` registers (`FrameBarrier::{None, Linked,
WriteThrough}`), and `note_frame_written(FrameId) -> WrittenFrame { lease,
submitted_at, barrier }` hands them back at the frame's write, so the plane
keeps no second table of frames in flight.

**M4.5-S35 amendment (ADR-0087 D2/D3):** the frame queue above is the
queued-frame FIFO. `note_frame_written` marks its frame written and
advances `written_up_to`/`written_bytes` over the **completion-ordered
written prefix** (a later frame landing first advances nothing — what an
fdatasync can honestly cover). `frame_plan(write_through_ok, seal_ahead)
-> FramePlan::{Seal(SealPlan), Wait(Hold)}`, with `SealPlan::{Plain,
WriteThrough, LinkedFsync}`, decides the next frame's barrier **before the
seal**, and every sealing path takes the `SealPlan` alone; its questions in
this order:
`Wait(Hold::ReorderWindow)` while the queue's own admission refuses (a late
front write and the frames that landed behind it fill the queue);
`Wait(Hold::SealUnfolded)` for a `seal_ahead` frame while the previous
rotation's seal is unfolded; `Plain` when no sync is due; write-through
under the prefix rule (pending write-through tickets of earlier in-flight
frames count as coverage; `seal_ahead` = a rotation's seal entry will
precede the frame and covers every queued byte) **and only while fewer
than `WRITE_THROUGH_WINDOW_ENTRIES` (16) write-through tickets sit
unfolded (ADR-0087 D2 third amendment) — behind a wedged FLUSH-class front
the arm declines and the frame seals `Plain`, its due accumulating**;
`Wait(Hold::Barrier)` when write-through is inadmissible and frames are
still in flight below (the frame is held ≤ one write latency — sealing it
barrier-less would starve the due); `LinkedFsync` only when `drained()`
(every earlier write completed — `IO_LINK` orders the sync after *this*
frame's write alone; release-asserted in `note_frame_queued`), the
FLUSH slot is free and the discretionary class has room — **the seal a
`seal_ahead` rotation registers counts as occupying the slot (ADR-0087
D3/D4 amendment): at bound 1 the rotation's frame seals `Plain`**; `Plain`
otherwise (the FLUSH slot busy or the discretionary class at its budget:
the due accumulates for the next group commit). `write_through_due()` / `frame_fsync_due()`
are now pre-seal predicates (the next base is `queued_bytes`).
`register_seal_fsync` asserts `drained()` — rotation is a drain point
(ADR-0087 D4; the plane holds a frame that needs rotation until no write is
in flight; `SegmentRotor::{rotation_due, next_frame_write_through_ok}` are
the pre-seal queries). `sync_due()`, `drained()`, `frames_unwritten()`.

**M4.5-S34 amendment (ADR-0086 D5):** `SyncReason::WriteThrough` (ticket
registered with its frame by `note_frame_queued`, completed from
`LogWritten`; `write_through_due()` = `sync_due` ∧ **the prefix rule** —
the frame's base equals the ledger's coverage tail, i.e. every byte below
it is durable or covered by a pending FLUSH-class entry; a FUA write
persists itself, never the un-barriered frames before it) and
`SyncReason::ZeroFill` (coverage-neutral, `register_zero_fill_barrier`).
`syncs_in_flight()` — the ADR-0022 D3 pipeline bound — counts FLUSH-class
entries only (O(1) on `flush_in_flight`; a `Linked` /
`Standalone` / `Completion` registration into a full pipeline is
release-refused). `write_through_latency_hist()` is the class-split
histogram; `CommitStats::{fsyncs_write_through, fsyncs_zero_fill}`.


`GroupCommit<File>` — cell-local policy engine + fsync ledger; never names
ops or sockets. The plane translates (choreography table in ADR-0013 D2 and
the module docs; `inf-server` adopts it at S08):

- Inputs: `note_staged(FsyncClass{Everysec,Always})` (EXECUTE),
  `note_everysec_tick()` (plane-armed injected timer — idle ticks free;
  a tick over staged records is never idle),
  `register_seal_fsync(SealHandoff)` (deferred rotation, ADR-0013 D4).
- LOG: `frame_plan(..)` before the seal (a `Wait(hold)` holds the frame,
  `Seal(plan)` seals it) → `note_frame_queued(end, lease, plan, now)`,
  which registers the barrier `plan` names with the frame (a `LinkedFsync`
  plan's one sync covers every class due that iteration — group commit);
  `standalone_fsync_due()` → `register_standalone_fsync()` (dirty bytes,
  no frame; covers written-at-submission, never queued).
- REAP: `note_frame_written(FrameId)` hands back the frame's lease (the
  release point), submit time and barrier; written prefix, ADR-0087 D2;
  `on_fsync_complete(ticket) -> Option<Lsn>` — submission-ordered ledger,
  **done-prefix** advance (completions may cross fds out of order);
  `on_fsync_error(ticket)` freezes the watermark forever (fsync failure is
  fail-stop — caller
  fail-stops; observable in tests).
- Watermark = **exclusive end** of the fsync-covered range; gate key =
  `Lsn::to_u64()` (`(segment << 32) | offset`, order-preserving). `always`
  futures register at their record LSN via `FrameLease::lsn_of` and wake
  FIFO by LSN under the EXECUTE budget (wake storms drain across slices).
- Counters (S21 vocabulary): `CommitStats`, `fsync_latency_hist` (µs),
  `pending_log_bytes`, `last_durable_lsn` (= watermark), `queued_up_to`.
- M4.5-S27 amendment (ADR-0083 D4): `rebase_clock(FsyncTicket, now)` —
  a *linked* fsync's latency clock restarts at its covering write's
  `LogWritten` (the `IO_LINK` sync starts only after the write), so
  `fsync_latency_hist` measures sync service time, never the
  write+sync chain. No-ops on completed/failed tickets.

## Segment lifecycle additions (M2-S05, ADR-0013 D4)

- `SegmentRotor::begin_frame_deferred → (FrameSlot, Option<SealHandoff>)`:
  rotation stays a pointer swap; the seal fdatasync rides the driver via
  the `#[must_use]` `SealHandoff{segment, end_offset, raw_fd()}`; the
  ledger drops the write handle when the seal's `Synced` arrives (seal
  durability and handle drop coincide). Sound because the staging lease
  serializes writes — rotation never happens with a write in flight
  (asserted at registration).
- `SegmentRotor::commit_frame_queued(slot)` advances the append cursor for
  driver-ridden writes; `active_raw_fd()` addresses them.
- `SegmentFile::raw_fd() -> Option<RawFd>` (`None` on `MemFs`; the
  reactor path requires a real-file tier — the sim disk hands out fake
  fds from a high base and the `inf-sim` `SimDriver` executes
  `LogWrite`/`Fdatasync` against it, S18/ADR-0020 D7: write completes
  `LogWritten` (page-cache, NOT durable), fsync flushes and completes
  `Synced`, a failed write cancels its linked sync with `ECANCELED`).

## `.ick` checkpoint format v1 (`inf-log::ckpt`, M2-S10, ADR-0016)

```text
header  := magic8 "INFICK1\0" · version u16 · cell u16 · ckpt_id u64 ·
           begin_lsn u64 (Lsn::to_u64) · ns_count u32 · ns_ids [u32] · crc u32
section := tag 0x01 · body_len u32 · record_count u32 ·
           body (record-v1 encodings) · crc u32       (CRC32C over tag..body)
footer  := tag 0x02 · section_count u32 · records_total u64 · ns_count u32 ·
           (ns_id u32 · entries u64)* · digest u64 · crc u32
```

- **The checkpoint is a materialized log prefix** (ADR-0016 D1): section
  bodies are ordinary record-v1 encodings, replayed by
  `Keyspace::apply_record` — the same upsert the tail uses. Strings emit
  `StringPostImage`; documents emit `DocFull` with canonical idoc bytes,
  exact lineage, and exact record version; either may be followed by
  `ExpireAt`.
  The checkpoint container stays v1 because only its extensible record
  vocabulary grew (M3-S17, ADR-0043 D7).
- `digest` = chained `inf_foundation::hash64` over the header CRC and each
  section CRC in order (seeded; part of the v1 wire contract). Recorded
  deviation from the originally planned "xxh3" (ADR-0016 D6); the version field is the
  upgrade path.
- Footer per-ns entry counts are S13's table-presizing input — realized by
  `read_ick_counts` (M2-S13, ADR-0018 D6): a header-hop footer peek (counts
  under the footer's own CRC; the streaming pass still runs the full audit)
  feeding `Keyspace::reserve_ns` before the bulk apply (measured: the
  doubling-rehash storm cost ~15% of replay throughput). Both the stream
  writer and footer-audit count `StringPostImage | DocFull` as namespace
  entries; metadata records do not inflate the presize count.
- **Section bound (ADR-0117 D1, 2026-09-12):** `ICK_MAX_SECTION_BYTES`
  (= `DEFAULT_MAX_FRAME_LEN` + `ICK_SECTION_SLACK`, 64 MiB + 4 KiB) is
  both `IckReaderConfig::default().max_section_bytes` and the writer's
  seal ceiling (`seal_section` release-asserts it). `IckStream::fits`
  is the walker's stage-or-seal test — true for an empty section
  unconditionally (one maximal record plus its expiry companion fits by
  the slack), else `body + bytes ≤ CkptConfig::section_bound` (default =
  the constant, asserted ≤ it; the DST lowers it). A walker that
  refuses an image resumes at it through the store's guarded in-chain
  cursor (`WalkCursor`/`ChainPos`, ADR-0117 D2). Every published `.ick`
  loads under the default reader.
- Writer tiers: `IckStream` (double-buffered section pair, `SectionLease`
  custody — the reactor tier rides `IoOp::LogWrite`/`Fdatasync` on the
  `.ick` fd with `TokenClass::CkptWrite/CkptSync`) and `SyncIckWriter`
  (tests/tooling over `SegmentFs`). Both produce identical bytes.
- Publication: stream to `ckpt-NNNNNN.ick.new`, then fdatasync → rename →
  dir-fsync (the `meta.rs` protocol class) — a file named `*.ick` is always
  footer-complete; `.new` orphans are S11 GC's job. S11's MANIFEST is the
  only authority that *names* checkpoints and must enforce
  `begin-LSN ≤ durability watermark` before publishing.
- Loader `read_ick`: validate-then-yield per section, footer audit
  (counts + digest + no trailing bytes); every structural error is typed
  and fail-stop for recovery. Fuzz target `ick_decode` (per-PR smoke +
  nightly hour).
- The document walker borrows tape images and freezes arena trees through
  per-store recycled output/walk/frame scratch. No per-entry allocation is
  introduced by a document-heavy checkpoint. The layout-independent state
  digest folds canonical idoc bytes, nonzero document lineage, **and the
  u24 document version**; physical form, cadence counters, addresses, and
  slack are excluded.
- `ckpt-begin` = record tag 5 (`RecordView::CkptBegin{ns: 0, ckpt_id}`),
  staged through the ordinary ring as `MutationEffect::CkptBegin` — its
  LSN resolves via `FrameLease::lsn_of` at LOG; replay counts it as
  `ReplayOutcome::SkippedMarker`. Decode rejects trailing payload bytes
  (`RecordDecodeError::TrailingBytes` — first fixed-width record payload).
- **M4-S12 amendment (ADR-0057 D3/D4) — `.ick` v2 + tag 8.** Cells owning
  tiered namespaces write header `version = 2`; tierless cells stay v1
  byte-identically (degenerate = absence). v2 adds block tag **0x03 —
  addr-ref section**: `tag · body_len u32 · entry_count u32 · ns u32 ·
  walk_watermark u64 · entry_count × (sidecar hash u64 · addr u48 LE) ·
  crc u32` — same {tag, body_len, count} header shape as 0x01, so the
  counts probe hops both; footer stays last and unchanged; per-ns entry
  counts include refs; the digest chain folds 0x03 CRCs in order. Decode
  audits shape + every `addr < walk_watermark` before the applier sees an
  entry; a records-only loader refuses hybrid files typed
  (`RefSectionUnsupported`). v2 tag registry, coordinated: 0x01 images ·
  0x02 footer · 0x03 addr-refs · 0x04 reserved (S14 live-set counters) ·
  0x05+ reserved (M4.5 index sidecars). Readers: `read_ick_hybrid` /
  `IckReader::next_step_hybrid` (per-section `IckRefSection` dispatch);
  writers: `IckStream::new_v2` + `stage_addr_ref`,
  `SyncIckWriter::create_v2` + `append_ref` (sections homogeneous by class,
  sealed at class/ns/watermark boundaries). Record tag **8 = `ColdDisplace
  {ns, old_addr: u48}`**: stages immediately before a tiered-namespace
  mutation that displaced a record; replay removes exactly the slot
  `(hash, old_addr)` then applies the mutation (zero disk reads — the
  hash-repoint and deferred-reconcile alternatives are rejected in the
  ADR); the walker never emits it; `Keyspace::apply_record` parks it
  until its paired mutation, which drains it (a marker of a namespace
  that is not tiered here — dropped, or a foreign log — is skipped with
  it). Fuzz: `ick_decode` extended over the v2 arm.
- **Tiered boot replay demotes when the RAM window fills (ADR-0174 D1,
  D3)**, and three rules above read so. Within one namespace every ref
  section precedes every image section: `IckStream` refuses to stage a
  ref for a namespace that has staged an image, and `apply_ref_section`
  refuses a ref section for a namespace that already holds a record of
  this life. A tag-8 marker naming an address below the life origin (the
  manifested `flushed`) removes that exact slot as above; one at or above
  it is a counted no-op (`markers_skipped`), and its mutation resolves by
  key. Replay reads cold bytes where it settles a pair: a tail `DEL`
  reads each same-hash cold slot at or above the life origin, and a RAM
  record about to be sealed, or left unsealed at the end of replay in a
  namespace that demoted, reads each same-hash cold slot. A read is the
  record's key window (`TIER_KEY_WINDOW_BYTES`) through the file's held
  creation-mode handle, parsed into `ColdKey`, whose constructor checks
  that the key hashes to the slot's hash; a failed read or parse is a
  typed boot refusal. `Keyspace::apply_record(rec, now, anchor, spill)`
  takes the replay seam (`ReplaySpill` lends the namespace's
  `TierReplay`; `NoSpill` lends none, and a record that needs room then
  refuses typed); the machine-holding entries are crate-private, so a
  machine reaches them only through it. A record that needs room changes
  nothing — no marker is drained, no slot moves — until the room exists.
- **M4-S14 amendment (ADR-0058 D3) — tag 0x04 activated.** v2 adds block
  tag **0x04 — live-set section**: `tag · body_len u32 · entry_count u32
  · ns u32 · entry_count × (file_id u32 · data_len u64 · dead_bytes u64
  · flags u8) · crc u32` — same header shape, so the counts probe hops
  it; one section per tiered namespace, emitted after that namespace's
  record/ref emission (counters cover attributions up to walk end).
  `flags` bit0 = byte-exact; unknown flag bits and `dead > len` are
  fail-stop at decode (`LiveSetSectionMalformed`). Entries count into
  `records_total` but **not** the per-ns presize counts (file entries
  are not index entries — both writer and audit agree). Loaders without
  the arm refuse typed (`LiveSetSectionUnsupported`). Readers:
  `read_ick_hybrid`/`next_step_hybrid` grew the third handler
  (`IckLiveSetSection` → `LiveSetFileEntry`); writers:
  `IckStream::stage_live_set`, `SyncIckWriter::append_live_set`. Restore
  clamps per ADR-0058 D5 (length-match or nothing; byte-exact only when
  fully dead) — dead bytes only ever under-count across recovery.
  Also under this amendment: `Index::position_of` now enforces the D4
  exact-pair discipline against the **full sidecar hash** for tiered
  tables (addresses are per-life, so `(tag, addr)` alone collides
  cross-key after recovery — the S14 sweep found the S12-era
  never-none violation and ADR-0058 D6 records the fix; memory-mode
  codegen unchanged). Fuzz: `ick_decode` extended over the 0x04 arm.
- **M4-S15 amendment (ADR-0059 D9) — displace replay is a bounded
  list.** Compaction relocations are unlogged, so a relocated record's
  WAL image can replay against a checkpoint that still refs the *old*
  address; `ColdDisplace`'s exact-slot removal would miss and the image
  would re-insert a stale twin. The next displacement of a relocated
  record therefore stages **one `ColdDisplace` per origin address**
  (from the table's bounded relocation-origin map, cap 3 per record;
  the compaction scan defers relocating a record already at cap) ahead
  of the ordinary marker, and replay's pending-displace register widens
  from a single slot to a bounded list (≤ 4 = origins + ordinary; the
  bound is a release assert). No format change — tag 8's shape is
  untouched; only the marker count per mutation and the replay register
  changed. Origin entries drop at covering swaps by walk stamp.
- Checkpoint I/O failure **aborts the checkpoint, never the process**
  (deliberately narrower than the log's fsync fail-stop rule — nothing
  was acked against the checkpoint and the log stays authoritative).
- Trigger v1 (ADR-0016 D7): cell-local `interval_bytes` threshold + the
  manual request epoch the cell owes, `CkptBoard::requested(cell)`
  (polled in MAINTAIN — the persisted-epoch pattern). One checkpoint in
  flight per cell; triggers latch, never stack. `INF.CKPT`/`BGSAVE` ride
  this.
- Manual requests are issued, never counted (ADR-0159 D1, A1):
  - A boot's partition (`inf_foundation::issue`) mints `N + 1` quotas of
    `floor((u64::MAX − N) / (N + 1))` units — one per cell, one for the
    control writer — and one final credit per cell, over a `CellCount`
    `N` (`1 ≤ N ≤ 16,384`). Only a credit advances the checkpoint clock,
    so every epoch is nonzero and above every earlier one of its boot; the
    clock cannot wrap.
  - A program reserves every unit it will issue before its first effect,
    from its own cell's quota: `INF.CKPT`/`BGSAVE` one, a durable
    namespace `CREATE`/`DROP` two (tombstone pacing and cleanup). An
    exhausted quota answers `-ERR checkpoint identity space exhausted`
    and changes nothing; it is permanent for the boot. The control
    writer's quota pays the boot tombstone restamp, then belongs to the
    host (direct admin, test and simulator requests). A cell's final
    credit issues its stop checkpoint, even past exhaustion.
  - The epoch is assigned when the credit issues (late), then raised into
    the all-cell request word or one cell's slot. Orderings (ADR-0159
    A1.6): the issue is an `AcqRel` `fetch_add`; every write to the word or
    a slot is a `Release` `fetch_max`; `requested(cell)` is the larger of
    two `Acquire` loads. An effect that happens before the issue of `e`
    happens before every checkpoint begun after a load of
    `requested(cell) ≥ e`. Loom checks it on the same types.
  - Cells never fold the whole board. Each cell sweeps it, at most 64 slots
    per MAINTAIN turn (`limits::CKPT_BOARD_VISITS_PER_TURN`, one Maintenance
    unit, MAINTAIN's first budgeted step), folding the minimum, a `u128`
    sum and the newest publication time; a completed sweep publishes its
    observation, and a changed sum wakes the parked `WAIT`s. A cell with
    a registered checkpoint waiter does not park while its sweep is
    part-way, nor while its own slot published since the start of its
    last completed sweep: a cell publishes after its sweep step, so the
    sweep that wakes its own waiter is the next turn's (at most
    `2 * ceil(N / 64)` unparked turns per own publication). A waiter
    whose last publisher is a peer is woken by the cell's next completed
    sweep after a park. All-cell `WAIT` and DROP pacing read the
    observation's minimum (a lower bound, never early); `WAIT CELL k`
    reads slot `k`. `LASTSAVE` and `rdb_last_save_time` read one per-cell
    value (`LastSave`): the observation's newest publication time, raised
    by every slot a `WAIT CELL k` on that cell confirmed, and written by
    those two steps alone. On one cell the two surfaces cannot differ;
    both can trail the board by up to two sweeps, except after a `WAIT` on
    the same cell. The catalog writer alone folds the whole board
    (tombstone retirement, one pass per persist on the control thread).

  > **Accepted 2026-09-21, implementation open — ADR-0145:** ADR-0145
  > adds a typed `RelocationPressure` cause beside Manual and Interval;
  > `interval_bytes = 0` now disables only the Interval cause. Not built.

### `.ick` container v3 (M4.5-S36 — ADR-0088 D3) — aligned blocks, direct writes

`ICK_VERSION_V3 = 3`: the v2 tag vocabulary (0x01–0x06) with **every
block — header, each section, footer — starting on an `ICK_BLOCK_ALIGN`
(= `FRAME_ALIGN`, 4096) boundary and zero-padded to the next one**.
Field layouts are unchanged (`body_len` is still the body length); the
reader hops `ick_align_up(SECTION_HEADER_LEN + body_len + CRC_LEN)` on v3
and the exact length on v1/v2 — one version-gated rule per hop site, the
footer probe included (it reads the last `ick_align_up(footer_len)`
bytes; the footer sits at the block's head). Padding is outside every
CRC's extent and **asserted zero** by every reader path
(`IckReadError::Padding` — the CRC's fail-stop class). The reactor tier
writes v3 on an `O_DIRECT` fd (`SegmentFs::create_meta_direct`, a
required trait method — a default would be a buffered file wearing a
direct label, ADR-0054 D3) from `Block` buffers whose content base is
4 KiB-aligned (the `FrameBuilder` shape; every growth re-bases). The sync
tier (`SyncIckWriter`) drives the same `IckStream` and produces
byte-identical v3 files (tests). v1/v2 files stay readable; no tool
writes them except tests and the sync tier on request.
`ckpt_padding_bytes` discloses the cost (≤ 4095 per block, ≤ 1.6 % at the 256 KiB
target). The release path shrinks an over-grown staging buffer back to
its nominal capacity (the v0.4.0 soak's 4× `ckpt_buffer_bytes` ratchet).

### Checkpoint trigger — derived (M4.5-S36 — ADR-0088 D4)

```text
interval_bytes = clamp(α × ckpt_bytes_last, floor, replay_bytes_per_s × replay_budget_s)
trigger        = frame_bytes_queued − at_begin ≥ interval_bytes
              ∨ records_appended − at_begin ≥ replay_records_per_s × replay_budget_s
α = 2 (DEFAULT_CKPT_ALPHA) · floor = CkptConfig::interval_bytes (256 MiB default; 0 = manual only)
replay_bytes_per_s = 1 GiB · replay_records_per_s = 400 k · replay_budget_s = 15 − 5 − 5
```

The accumulator is **on-disk frame bytes** (header, trailer, v3 padding
— ADR-0086 D3's obligation), not staged record bytes; before the first
checkpoint the estimate is the cell's live record + document bytes
(an over-estimate for tiered cells delays the first checkpoint toward
the cap, which bounds recovery); `derive_interval` release-asserts
`floor ≤ interval ≤ cap`. INFO: `ckpt_interval_bytes`,
`ckpt_records_since_begin`, `ckpt_bytes_total/last`,
`ckpt_padding_bytes`, `manifest_bytes_total`, `log_frame_bytes`, and the figure
`write_amp_milli_log_checkpoint` (+ `_undefined`, ADR-0060 D3 rule).

> **Accepted 2026-10-19, implementation open — ADR-0180:** ADR-0180 restates
> this section's rule and the ADR-0145 note above it in one place. Before
> the first checkpoint the interval is the floor alone, not a live-bytes
> estimate. `derive_interval` asserts `floor ≤ interval`, and
> `interval ≤ max(cap, floor)` when a cap exists: a cap below the floor
> yields the floor. `replay_bytes_per_s` is the probed read rows' per-cell
> share (1 GiB/s only on an unprobed node). `interval_bytes = 0` disables
> the interval cause only; the relocation-pressure cause is not built.

## Device budget (`inf-runtime::budget`, M4.5-S36 — ADR-0088 D1/D2b, ADR-0178)

```rust
pub enum IoClass { LogFrame, BlobWrite, ColdReadForeground,      // foreground: charged, never deferred
                   ZeroFill, TierFlush, Checkpoint, ColdReadMaintain } // background: priority order, weights 4:4:2:1
pub struct DeviceModel { write_bytes_per_s, write_ops_per_s, read_bytes_per_s, read_ops_per_s } // 0 = unbudgeted
pub enum Issue { Now, NotThisSlice }                                // a background producer's only answer
pub struct ClassCap { bytes, ops }                                  // the most a class's own credit holds
impl DeviceBudget { fn refill(&mut self, now: Nanos);               // once per MAINTAIN entry
                    fn offer(&mut self, class, bytes, ops) -> Issue; // the one background entry point
                    fn charge(&mut self, class, bytes, ops);         // unconditional; owes what credit cannot hold
                    fn refund(&mut self, class, bytes, ops);         // the unissued part of the grant just before it
                    fn cap(&self, class) -> ClassCap; }
pub struct SealPace;  // take(now, held) -> bool: a second frame seals at the cell's share of write_ops_per_s_4k_qd4
```

**The grant rule (ADR-0178 D2).** Per cell, per direction (write, read)
and per axis (bytes, ops): the grant for the elapsed interval is the
cell's share (`model / cells`, computed once at boot — L1) minus the
foreground's spend since the last refill, clamped at `≥ share /
FLOOR_DIVISOR` (8); carries keep every sub-unit remainder. It is split by
weight into per-class credit capped at `cap_c = max(slice_c, share ×
w_c/Σw × BURST_HORIZON_NS)` (50 ms, derived from the S27 D5 bar), and what
a capped class cannot hold overflows into a per-direction pool (cap
`share × horizon`). On the checkpoint's byte axis the grant is floored at
the log's bytes since the last refill over the trigger's α (the keep-up
floor): the floor and the weighted share are compared exactly and
rounded once, with one remainder carried (ADR-0178 D2).

- An offer at most `cap_c` on every budgeted axis is **attainable**: it is
  granted from the class's credit, then the pool, or answered
  `NotThisSlice` — the producer keeps it and re-offers next slice, with
  nothing moved. A class that owes is granted nothing.
- An offer above `cap_c` on a budgeted axis can never be covered by the
  class's own credit, and the pool is not its to wait on: another class
  may always drain it first. It is issued by a counted **overrun** once
  the class is *rested* at its cap — its credit, then what the pool holds,
  and the rest is owed, repaid by refills before the credit grows again.
  A class is rested when every budgeted axis held its cap before a
  refill's grant. Refill alone decides it, so a draw ends the rest pass
  (the class drops below its cap) and a grant refunded in full does not.
- `charge` (a checkpoint's header block and barriers, any foreground op)
  spends unconditionally and owes what the credit cannot hold. The tier
  class has no charge site: every tier offer is a group at its exact size.
- The budget keeps the last background `Now` answer's draws. The refund
  directly after it returns at most that — the debt first, then the pool,
  then the credit — so a full refund is the grant's exact inverse. Any
  other call voids it, and a refund without it returns nothing.

Producers match `Issue` and nothing else; the three outcomes and the
overrun are resolved in `offer`, once. Consult sites: zero-fill
(`next_zero_slice` peek → offer → push), tier flush (ADR-0178 D3's tier
row: the cell's turn offers each ready namespace's next **group** — the
longest run of its staged round's unsent ops within 1 MiB of device
bytes and 64 ops, whole writes, `TIER_GROUP_BYTES_MAX` and
`TIER_GROUP_OPS_MAX` in `inf-log::limits` — at its exact size, so a
round issues exactly what it offers and nothing is settled afterwards; a
refused group makes its namespace the holder, which ends the pass for the
tier class and is visited first next turn, while a granted one moves the
visit on; barriers are their own group once every write is done, and a
failed write is unsent again and offered as a retry group, charged like
any; the cell plans at most one round per MAINTAIN turn, taken by the
first namespace after the one planned last, in id order, that is idle
with work at its cursor — `TieredTable::flush_pending`, so nothing at the
cursor means no plan and no offer),
checkpoint (header/section/footer block at its padded length, before the
seal; completion fdatasync charged as one op), cold-read drain
(`drain_budgeted`: maintain reads offer the pool buffer bound and refund
the unused window; foreground reads are charged). Charges: `queue_frame`
(`LogFrame`, bytes + 1 op, +1 for a linked sync), `write_blob`
(`BlobWrite`), MANIFEST barriers (`Checkpoint`). Model absent ⇒ every
offer `Now`, every counter counts, the checkpoint keeps its 64 MiB/s pace
(ADR-0017) — reported as `io_budget_model:absent`. No client reply is
ever derived from `NotThisSlice` or an overrun.

**Progress (ADR-0178 D4).** A class with one producer on the cell —
zero-fill, the checkpoint, the cold-read drain — has every offer issued
within `max over budgeted axes of (O + cap_c + C_c + 3) / r_c + 2Δ` of
injected time. `r_c = share × w_c / (8 Σw)`, and on the checkpoint's
byte axis `share × max(w_c / (8 Σw), w_c / (α w_c + Σw))` (`share / 7`
at α = 2). `O` is the class's debt when the offer is first made, at most
`(largest offer − cap_c)⁺ + C_c`; `C_c` its unconditional charges
between two offers (the checkpoint's header block and barrier ops; 0 for
the others); the 3 units cover the carries' lag — each carry stage (the
rate product, the ⅛ floor, the weighted split or the keep-up floor's one
carry) trails its exact sum by under one unit, under 2⅛ in all; `Δ` is
the longest refill interval — one for quantization, one for the rest
pass. The tier class never owes (no offer above the cap, no charge
site): a ready group is issued within `n × T_c`, `T_c = max over
budgeted axes of (B + 3) / r_c + 2Δ` with `B` the group's own size and
`n` the cell's tiered namespaces, and an idle namespace with work plans
within `n − 1` further cell turns. INFO reads the longest group wait as
`tier_group_wait_ns_max`, the turn's refusal episodes as
`tier_turn_waits`, the contended turns as `tier_turn_contended`, the
namespaces a plan passed over as `tier_plan_waits`, the rounds planned
as `tier_rounds_planned` and the refused groups as
`tier_flush_groups_deferred`. It is a progress bound, not a latency
promise: a writer parked behind a round whose groups take longer than
its namespace's `TAIL-STALL-TIMEOUT` takes the typed timeout.

**The limit** (ADR-0178 D4):

- *Checkpoint block — a limit.* A section block holds one record plus
  less than one section target. Above the class cap it issues as one
  burst of up to the record bound (≤ 64 MiB + 8 KiB at the largest
  `--log-staging-mib`), the same device time the log already spent
  writing that record as one frame. `--log-staging-mib` owns it. The
  checkpoint is the only producer that can overrun, and it is alone in
  its class.
- *Tier flush.* One group, at most 1 MiB of device bytes, behind a
  foreground frame: 2.1 ms at 489 MB/s, 8.4 ms at 125 MB/s, inside the
  50 ms premise at any `MAINTAIN-SLICE`; no tier offer is above the class
  cap, so `io_budget_unattainable_tier_flush` and
  `io_budget_overrun_bytes_tier_flush` read 0.

INFO, per class and cell scope: `io_budget_bytes_c`, `io_budget_ops_c`,
`io_budget_deferrals_c` (every `NotThisSlice`), `io_budget_unattainable_c`
(offers above the cap, repeats included) and `io_budget_overrun_bytes_c`
(bytes issued overruns offered above the cap, cumulative).

`SealPace` (ADR-0088 D2b): a token bucket at `write_ops_per_s_4k_qd4 /
cells`, capacity K. The LOG step asks only when `!staging.drained()`;
a refusal holds the frame (it keeps accumulating; `frame_waits_pace`
per episode). A drained cell always seals — never slower than K = 1.

## Cap census and `INFO caps` (`inf-foundation::bounded`, `inf-server` — ADR-0163 D2)

Every capped container of a cell registers a row of that cell's
`CapCensus`: its cap name, its cap, its fill (`Serving`: filled while the
cell serves; `Assembly`: filled whole before the serve mark), its
`high_water` (the most entries one instance has held, the maximum over the
row's instances in the cell), its `crossings` (`Full` answers and cap
merges, summed over them) and its `capacity_now` (the cap, less the slots a
`Retire` slab retired at their last generation: the minimum over the
instances, so a row that crossed holds `capacity_now ≤ high_water`).
Instances of one cap name share one row.
`ServerPlane::new` builds the census once, with its cell's id, beside its
one `ServeMark`; the cell's loop takes the mark as its first iteration
begins (the census goes `Assembling` → `Serving`). At the mark an
`Assembly` row whose live count is below its cap is a violation, and so
is any publish, crossing or registration on an `Assembly` row after it:
each counts in `cap_assembly_violations` (cell scope) and is a debug
assertion carrying `cap-assembly-violated`. Each of the cell's two
aggregates that own its containers, its loop (`CellLoop`) and its plane
(`ServerPlane`), marks the census `Stopping` in its `Drop`, before its
fields drop; the mark is idempotent, so the order between the two is no
one's. A slab's slot store (ADR-0171 D3) dropped holding a chunk or a
directory while the census is `Serving` is a release
no step sliced: it counts in `cap_unsliced_drops` (cell scope) and is a
debug assertion carrying `unsliced-drop`, outside an unwind. Dropped while
`Assembling` it is an abandoned assembly, once `Stopping` process teardown;
neither counts.

`INFO caps`, cell scope (a node fold is the reader's sum):
`caps_scope:cell`, `cap_census_phase` (`assembling`, `serving`,
`stopping`), `cap_assembly_violations`, `cap_unsliced_drops`, `cap_rows`,
and per row
`cap_row_<name>:entries_max=…,fill=…,high_water=…,crossings=…,capacity_now=…`.
A harness that attaches no census renders no section.

**Extent-reclaim marks (`epoch-marks`, ADR-0061 D5).** Each tiered
namespace's `(seq, epoch)` reclaim marks are a
`CappedDeque<EpochMark, Coalesce<LaterMark>>` over `EPOCH_MARKS_MAX` = 16
(`inf-server::limits`), one per ack class. MAINTAIN reads the durable cell's
staged seq, watermark and `ack_class(seq)` → `Covered | Frame(last seq) |
Builder` (the ack-map entry that will release the seq, by one binary search;
the builder when none). A tick pops the covered front marks; then, with the
staged seq and the namespace's WAL epoch both past the back mark, adjacent
marks of one class merge into the later, the pair replaces a back of its own
class or is pushed, and at 16 classes it merges into the back (`LaterMark`
takes the later pair whole: the back's deaths wait for the later class's
barrier, a crossing on the row); the covered front pops again. A merge never
makes reclaim earlier than the uncapped rule's.

**Extent-seal window (`extent-window`, ADR-0061 D3).** A blob `SET`'s extent
barrier holds the extent's open file until the barrier folds into the
ledger's durable prefix, so each cell's `GroupCommit` bounds its extent
barriers with a `CappedDeque<ExtentSync, Reserve>` over
`EXTENT_SEAL_WINDOW_ENTRIES` = 16 (`inf-log::limits`): one `(ticket, fd)`
entry per extent barrier registered and not folded, in ledger order.
`GroupCommit::register_extent` reserves a unit before the effect that makes
the extent (its id, its file, its bytes, the store's reference) and
publishes the ledger entry and the window entry in one step; a refused
effect gives the unit back; the fold frees it; MAINTAIN issues each entry's
fdatasync once, at most 16 a tick. Who reserves is the durable cell's
`ExtentWaiters`: `len` window entries, `w` turns out, `q` writers parked.
An arrival (a blob writer with no turn) reserves only while the window is
open and no writer is parked and, with turns out, while `len + w < 16`;
else it parks in FIFO order with nothing created. The LOG step that holds
the staged frame for an outstanding extent barrier closes the window; the
step that seals that frame, or finds nothing staged, opens it and wakes
`min(16 − len − w, q)` writers from the queue's head, each a turn. A woken
writer resolves its key once more and reserves with its turn — every turn
has a unit (`len + w ≤ 16`) — and publishes or leaves; a turn that leaves
gives its unit back, to the queue's head while the window is open. A fold
wakes no one. A writer that parks on staging or the tail stall gives its
turn back first. The pacing is latency only: no reply of its own.

INFO, cell scope: `frame_waits_extent` (`# Persistence`, beside the other
`frame_waits_*`: frames held for an extent barrier, one per closing),
`extent_parks` (blob writers' park episodes) and `turn_refused` (turns that
found the window full, 0 by the arithmetic above), beside
`fsync_latency_p99_us`; the window's row in `INFO caps`, whose `crossings`
count the arrivals that found it full.

## Boot recovery orchestration (`inf-server::recover`/`plane`, M2-S15 — ADR-0019)

- Recovery is a **resumable state machine**: `Recovery<F: SegmentFs>` with
  phases Start → Ick → Replay → Audit → Finish, each `step(ks, budget)`
  bounded by input bytes (one frame/section overshoot). `open_cell_log`
  is the machine run to completion — one code path. The determinism
  sweep and the taxonomy suite run it in one step (a `u64::MAX` budget),
  so they exercise no yield; the stepped drive is exercised by the
  server's step tests and the sliced end-to-end tests.
  `IckReader` is the pull-based `.ick` loader (`read_ick` reimplemented
  on it; same audit, same fuzz target).
- The node boots cells directly into their reactor loops:
  `ServerPlane::begin_recovery` (requires `set_control`) drives steps from
  MAINTAIN (`RecoverConfig{step_bytes: 8 MiB, throttle_bytes_per_sec:
  None}` — the throttle is test-only, metered on **consumed** bytes, never
  the prealloc-slack credits progress also carries). Unthrottled boots
  never park (`before_park`); recovery failure surfaces via
  `take_boot_error` → the assembly fail-stops the process.
- **`-LOADING` gate (wire layer):** `CmdFlags::LOADING` in the command
  registry — membership pinned to *observed* Redis 8.0.5 behavior
  (captured 2026-07-03; notably **PING is gated** there). While `RecoveryBoard::all_ready()`
  is false, non-LOADING commands answer the exact Redis bytes
  (`-LOADING Redis is loading the dataset in memory`); unknown commands
  resolve first (Redis order). Node-scoped: a recovered cell still
  answers `-LOADING` while a peer replays.
- `RecoveryBoard` (on `ControlHandle`, one slot per cell): single-writer
  progress atomics (bytes/segments done+total, records, torn LSN) — the
  L1 control-plane carve-out class, like the park board. Feeds the
  control thread's per-cell/aggregate boot log lines and the `INFO
  persistence` loading fields (`loading`, `loading_start_time`,
  `loading_total_bytes`, `loading_loaded_bytes`, `loading_loaded_perc`,
  `loading_eta_seconds` + extension `loading_cells_ready`/
  `loading_cells`; totals are file extents incl. prealloc slack —
  disclosed upper bound).

## Fault-point registry (`inf-foundation::fault` + `inf-log::fault`, M2-S16 — ADR-0019)

- Registry (`inf_foundation::fault`): **thread-local** — cells are
  single-threaded (L1), arming happens on the firing thread (test body,
  or a per-cell fault plan applied at cell boot). API: `arm(point,
  FaultSpec)`, `disarm/disarm_all`, `fire(point) -> bool`,
  `occurrences/fired(point)`. Triggers (all deterministic — L7):
  `Always | Nth(n) | FromNth(n) | Probability{num, den, seed}` (seeded
  `SplitMix64`; occurrence counts start at arming).
- **Cost contract:** everything is behind the `fault-points` feature
  (OFF in shipping builds; test builds enable it via dev-dependency
  unification). Compiled out, `fire` is a `const false` — measured
  **0.000 ns/call** and the release `infinityd` binary carries no
  machinery strings (dev-box A/B, 2026-07-03). Feature-on unarmed:
  0.47 ns/call (one TLS read + branch).
- Point names are declared by the owning crate: `inf_log::fault` v1 set
  `log_append_short_write`, `torn_frame` (prefix lands, call *succeeds* —
  lying-disk physics; meaningful only as the final pre-crash write),
  `fsync_err`, `manifest_rename_fail`, `dir_fsync_fail` (all three
  barrier classes: boot dirs, segment prealloc, envelope swap step 6),
  `power_cut_after_seal` (typed error standing in for death after a
  durable seal), `prealloc_no_space` — plus the `ALL` inventory.
  **S17 addition (ADR-0020 D3):** `inf_server::fault` declares
  `durable_fsync_eio` — fired in `DurableCell::on_synced`, it converts a
  successful fsync completion into the device-reported-EIO path (the
  reactor tier's `fsync_err` analog: on a live node the seal fsync is
  deferred through the driver — ADR-0013 D4 — so fsync failure is a
  CQE, and this point is its deterministic stand-in). Firing it
  freezes the watermark and fail-stops the process with
  `EXIT_DURABLE_FAILSTOP` (= 3).
- CI coverage check `scripts/check-fault-points.sh` (in `just check` +
  CI): discovers every `crates/*/src/fault.rs` declaration module; every
  declared point must be fired in library code AND exercised by ≥ 1
  test (crate `tests/` trees or workspace test crates under `tests/*`) —
  an unexercised point fails the build. The M2-S17 crash matrix
  (`tests/crash-matrix/{m2,m4,m45}.toml`) additionally requires a matrix
  row per point (runner-enforced). Node rows name an exact
  `package::target::test_function`; the runner executes it and requires
  a fresh point/verdict receipt after its assertions (ADR-0020 amendment).
  Missing, ignored and empty carriers fail. The M2-S18 sim disk consumes
  the same registry for power-cut scheduling (arm a point → observe the
  typed error → `cut_after_ops`/`power_cut`); reactor-tier write/fsync
  failures are injected by the ScriptedDriver, the sim disk's dead switch,
  and `durable_fsync_eio`.

## MANIFEST schema v1 (`inf-log::manifest`, M2-S11 — ADR-0017)

- `shard-k/MANIFEST` names the recovery unit atomically:
  `{format epoch, ckpt id, begin-LSN, live segment set}` inside the
  `inf-log::meta` envelope (magic `INFMETA1` + length + CRC32C — one swap
  protocol shared with the catalog; the reader refuses any envelope file
  over `MAX_ENVELOPE_LEN` = 64 MiB **by inode length, before allocating**).

  > **Accepted 2026-09-23, implementation open — ADR-0158:** reserved
  > catalog publication and recovery headroom gives META its own envelope
  > bound, `CATALOG_ENVELOPE_BYTES_MAX` = 70,837,137 B, owned by one
  > byte-limit table in `inf-log::meta` and selected by envelope kind, not
  > by caller or filename. MANIFEST keeps the 64 MiB bound above. Ordinary
  > growth still ends at or below 64 MiB; the extra band holds legacy
  > normalization and required cleanup. A META above 64 MiB is refused by
  > binaries built before ADR-0158.

  > **Accepted 2026-10-19, implementation open — ADR-0182:** ADR-0182
  > restates ADR-0158 whole and is the META envelope contract once
  > accepted: the bound and its band are unchanged. A META above 64 MiB is
  > refused by binaries built before ADR-0158, a recorded deviation (expiry
  > 2026-12-31, or the first public tag if that comes earlier), after which
  > it is a documented format boundary. Not built.

  Payload: magic `INFMAN1\0`, `epoch: u32 = 1`, `ckpt_id: u64`, packed
  `begin: u64`, count + u32 segment ids (strictly ascending; `segments[0]
  == begin.segment` = the truncation floor). Canonical decode: trailing
  bytes / empty or non-ascending sets / floor mismatch are named errors.
  v1 writes the contiguous `floor..=active`; decode admits holes (M5/M7
  publish them).
- Swap = write-new + fdatasync + rename + dir-fsync, always. On the
  reactor tier the fsync-class steps ride `BackendDriver` as
  `TokenClass::ManifestSync` barriers (one in flight per cell) and the
  staging create is barrier-free (`SegmentFs::create_meta`); the
  synchronous `write_manifest` remains for tests/control tiers — same
  bytes, same step order, same crash windows.
- Publication guard: the swap is staged only once the durability
  watermark covers `begin-LSN`; one recovery-unit transition in flight,
  ever; the interval trigger re-bases to the **begin-time** staging total
  (a publish-time rebase lets a paced walk's lag escape the trigger —
  unbounded retained log; ADR-0017 D2).
- Truncation: sealed segments below the durable floor are forgotten by
  the rotor and their unlinks **delegated to the control thread**
  (freeing pages is O(size) — a measured loop stall); `.ick` GC keeps
  exactly the named id. ≤ 2 unlink/GC ops per MAINTAIN slice; no
  dir-fsync after unlinks (below-floor resurrections are boot-GC'd).
- Recovery (completed at S13/S14, ADR-0018): manifest → floor-aware scan
  (`scan_log_dir_from`; gaps below the floor are stale, above it fatal) →
  `.ick` footer-count presize → named `.ick` loaded via `read_ick` through
  `Keyspace::apply_record` (header cross-checked: id/begin/cell) → tail
  replay from `begin` (floor-segment records below begin are skipped — an
  older post-image must not regress checkpointed state) → per-segment
  slack audit (M2-S14: a validating self-located frame beyond any
  segment's data end = `LogCorruption` fail-stop; residue in the resume
  region = torn-tail truncation of the tail pointer + trailing-segment GC;
  sealed-slack residue tolerated + counted) → begin-LSN guard (valid data
  ending below `begin` = missing covered state, fail-stop) → boot GC.
  **Recycled-life residue (M4.5-S39b, ADR-0090 D2 as amended):** replay
  ends a segment's data at the first `ForeignSegment` frame (a classified
  end like `ZeroTail`; `RecoverStats::segment_residue_stops`; replay
  continues in the next segment — unlike the epoch residue stop). The
  audit's `RegionEvidence` carries `foreign_frames` / `max_foreign_epoch`
  beside the self-located facts; foreign frames never contribute
  attestation, epoch or hole evidence and are skipped by their padded
  extent **only after their CRC passed** (a stale header over a partly
  overwritten body fails the CRC and is scanned byte-wise, so a same-
  segment frame can never hide behind a foreign length field). A slack
  with no self-located frame and ≥ 1 foreign frame is **proven recycled
  residue** (`recycled_residue_slacks`): never a hole, never torn —
  trailing segments behind the resume point are removed as stale. A slack
  with a self-located frame is a hole of this life at that frame exactly
  as before, foreign frames or not. The resume epoch is `1 + max(prefix,
  beyond-frame, foreign)` epochs.
  Manifest present but named state missing/corrupt = fail-stop, never a
  full-replay fallback. `open_cell_log` is generic over `SegmentFs` (DST
  seam); recovery is digest-deterministic (`Keyspace::state_digest`,
  same-files ⇒ same digest + same resume LSN, CI-asserted).
  ADR-0137 adds the simulator's separate `state_hash`: event/step clocks,
  boot/ready/end boundaries, recovered keyspace digests and disk-image
  names/bytes. The apply-trace encoding and release trace baselines stay
  unchanged; insertion debug assertions no longer inflate cold-read counts.
  `TieredTable::simulation_digest` is a read-only diagnostic over every
  index hash/address and resident record; the disk digest covers cold
  bytes. It starts no checkpoint walk and changes no runtime state.
- Checkpoint streaming is paced: `CkptConfig::stream_bytes_per_sec`
  (default 64 MiB/s, injected clock; 0 = unpaced) — burst walks trip
  kernel dirty-page throttling and stall the log write's CQE path
  (the S12-measured foreground cliff).
- Counters: `manifests_published`, `segments_truncated`,
  `log_segments_live` join `INFO persistence` (S21 vocabulary).
  M3-S17 adds `doc_deltas_skipped_stale` and
  `doc_deltas_skipped_missing`; fuzzy-checkpoint overlap skips are
  observable facts, never silent replay drops.
- M2-S22 additions to `INFO persistence` (additive, campaign observables):
  `log_frames_queued` (cumulative frames handed to the LOG writev — the
  `log_writes_per_iter` tripwire numerator over `raw_iterations`) and
  `log_staging_bytes` (staging domain resident bytes, 2 × buffer capacity
  by construction — the L5 attribution term the S22 durable fill leg sums).
- Fuzz: `manifest_decode` (envelope + payload, canonicality oracle) —
  per-PR smoke + nightly hour alongside the other decoders.
- **M4-S12 amendment (ADR-0057 D5/D6) — epoch 2 tier sections.** The
  `epoch` field bumps to 2 when tier sections are present; tierless cells
  keep writing epoch-1 payloads byte-identically, and the v2 reader
  accepts both (v1 readers refuse epoch 2 typed). v2 appends
  `tier_ns_count u32` + per-namespace `{ns u32, flushed u64 (48-bit),
  file_count u32, file_count × {id u32, base u64, durable_len u64}}` —
  canonical: namespaces and file ids/bases strictly ascending,
  `durable_len ≥ 1`, ranges non-overlapping and tiling inside
  `[0, flushed)` (ring-top gaps and a trailing gap legal). The manifest
  names **logical durable ranges**, not physical files: a sealed file's
  physical excess (the capacity-seal edge) is inert; recovery truncates
  only unsealed files. `flushed` is the next boot life's origin;
  `ckpt_id` doubles as S15's deletion-stamp sequence. Recovery:
  `inf_store::recover_tiered_ns` (probe sealed fast-path via
  `probe_tier_file` — two block reads, frames verify lazily on read;
  unsealed → `recover_seal_existing` at the manifested length; un-named
  cold files deleted) + `apply_ref_section` (walk-watermark ≤ manifested
  flushed cross-check). Writer input: `TieredTable::tier_manifest`
  (catalog clamped to `flushed`; zero-confirmed files not named).
  `TierFlush::with_catalog` seeds a recovered pipeline. Fuzz:
  `manifest_decode` extended over epoch 2 with the tiling invariants.
- **Every tiered namespace in the catalog recovers through a manifest
  section (ADR-0174 D2, D4, D5).** One that no section names (no MANIFEST
  yet, or one published before the namespace was created) recovers
  through the empty section (`flushed` 0, no files, next file id 0), so
  its tier files are removed before any flush; a tombstoned namespace's
  directory is swept with or without a MANIFEST. A replay allocation the
  window refuses demotes through the recovered pipeline (`BootFlush`, on
  the seam drive: seal one `MAINTAIN-SLICE` past the need, one flush with
  one barrier the boot pipeline claims whole, release), padding the tail
  to the ring top or the next commit page when the need lies above it; it
  never fails the boot. The recovery driver drains each machine's
  `ReplayWork` and charges each step the boot I/O it did (tier bytes and
  the bytes the end settle walked at one budget byte each, 4 MiB per
  barrier — `inf-server` `limits` — and 128 KiB per settle read, the
  store's `SETTLE_READ_CHARGE_BYTES`, on which the end settle yields) and
  yields at the next frame or checkpoint section once the charge and the
  bytes read reach the step budget. At the end of the checkpoint each
  address a settle chained releases its blob reference. Once the finish
  step declines a lift, an unpaired marker refuses the boot; then the
  `Settle` phase settles every RAM record a namespace that demoted left
  unsealed, one budgeted step at a time; then the hand-over seals the
  active file and the plane receives a pipeline under the live claim
  rule with a handle for every sealed file, so a boot-demoted key is
  readable by the first command. A boot that fits every window writes
  only the `Recovered` reseal and leaves the zero set at zero.
  Boot-written files stay unmanifested until the next checkpoint
  publishes, and a crash leaves them for the next boot to remove. A
  live-set entry naming a file this boot created restores nothing.
  Counters (per cell in `RecoverStats::tier_replay`, the node fold in
  `INFO persistence`, prefix `recover_node_tier_`): demote steps, pads,
  tier bytes written, barriers, files sealed, settle reads, same-key and
  distinct settles, deletes verified, blob releases (the zero set);
  markers skipped and dead-life files removed (outside it); the largest
  step charge (a gauge, the largest over the cells).
- > **Accepted 2026-10-04, implementation open — ADR-0174 A1:** recovery
  > resumes inside a frame and yields after bounded work. A step that has
  > charged its budget stops at the next record or checkpoint entry, and
  > the next step resumes there through a typed cursor; the read and audit
  > of one frame or one section stay one unit, bounded by the decoder's
  > cap. The yield sentence above then reads: the driver yields at the next
  > record or entry once the step's charge reaches the budget. A charge is
  > budget bytes, not elapsed time and not memory. From that build, its
  > walked-byte price covers the bytes a demote step's seal walk passes as
  > well as the end settle's; until then it covers the end settle's alone.
  > `Keyspace::apply_record` (the tag-8 bullet above) answers
  > one of three: consumed, consumed after priced boot work, or
  > `NeedsRoom(ns)`; on `NeedsRoom` the record has changed nothing, and the
  > driver runs the demotion under the step budget and presents the record
  > again. The checkpoint reader announces an entered section, and a
  > handler answers next or park (an entry handler also how many entries
  > it applied); a parked step keeps its cursor at the item not consumed.
  > The segment reader re-issues the frame it holds for a cursor its own
  > record iterator produced; a cursor whose frame is no longer held is a
  > typed boot refusal. The counter list gains, beside the largest step
  > charge, the largest work one step did (its charge plus the bytes of the
  > frame or section it parked in) and the largest loop-clock interval
  > between two consecutive steps, in nanoseconds, recorded and not gated;
  > and two liveness counters outside the zero set: the steps taken, and
  > the yields inside a unit by kind (frame, section, demote step). Not
  > built.
- > **Accepted 2026-09-23, implementation open — ADR-0156:** bounded,
  > acknowledged tier-file retirement replaces the plane-layer unlink
  > below with an owned cell→control job: eight positions per cell cover
  > request, worker and unconsumed result; the worker closes the final
  > handle then unlinks; `Unlinked` and `AlreadyAbsent` are terminal,
  > `Retry` returns the owner for paced retry; one 64-visit allowance per
  > turn. Eligibility is unchanged.
- **M4-S15 amendment (ADR-0059) — retirement + unlink lifecycle.** The
  deletion conjunction made mechanical, staged around the MANIFEST
  swap: `TieredTable::begin_ckpt_walk(ckpt_id: u64)` (signature grew
  the id — it stamps the live set's `ckpt_begun`) → walk → `retire_scan
  (ckpt_id, flush)` marks files `retiring` when `is_dead ∧ unref_stamp
  < ckpt_id ∧ sealed` (`unref_stamp` records the last-begun walk at
  **every** slot-removal naming the file — relocations and user deaths
  alike) → `tier_manifest` **excludes** retiring files from the new
  unit → swap success ⇒ `commit_retirement() -> Vec<u32>` / failure ⇒
  `abort_retirement()` (marks roll back; nothing unlinks). Unlink is
  plane-layer: `TierFlush::detach_sealed(id) -> Option<TierFileMeta>` +
  `inf_log::flush::unlink_tier_file(fs, &meta)` (fault point
  `tier_unlink_fail`, **non-fatal** — reclaim defers and the ADR-0057
  D6-1 boot GC redrives it: any cold file the resolved manifest does
  not name is deleted at recovery). Read pins (`ColdReads::inflight_on`)
  additionally defer the physical unlink. ADR-0057's tiling language is
  amended by ADR-0059 D5: retired interior files leave **legal gaps**
  in the manifest unit (ranges stay non-overlapping and ascending — no
  format change; epoch stays 2). The reclaim coordinate is the **cold
  floor** (lowest surviving file's base, `TieredTable::cold_floor()`)
  — derived, not a fifth watermark. `recover_tiered_ns` grew a
  `boot_ckpt_id` parameter seeding recovered files' stamps.
- **M4-S16 amendment (ADR-0060) — the write-amplification numerator, and
  a third bound on the flush chunk.** Two changes to already-frozen
  vocabulary:
  1. `WriteAccounting::written_bytes()` is `wal_bytes + flush_bytes` (was
     `+ compaction_bytes`), and `compaction_bytes` is **relocation
     volume**, not a device-byte leg: ADR-0059 D2's relocation is a
     verbatim placement into the RAM tail, so those bytes reach the
     device through the ordinary flush leg where `flush_bytes` counts
     them. Settled by the block layer (ADR-0060 D2): with compaction
     active the accepted numerator reconciles at −2.27% while the
     three-leg sum misses by +13.15%. New API:
     `WriteAccounting::write_amplification() -> WriteAmplification`
     (`Measured { milli }` | `Undefined { written_bytes }`; milli-units,
     ceiling-rounded, saturating upward — a reported figure never
     understates), `Keyspace::tiering_write_amp() -> WriteAmpSummary`
     (worst namespace + unbounded count), and
     `Keyspace::tiering_write_accounting()` now returns
     `WriteAccountingTotals` — a **ratio-free** aggregate type, so a
     blended node-wide figure is unrepresentable rather than merely
     discouraged. `INFO tiering` gained `tiering_write_amp_milli_max`,
     `tiering_write_amp_undefined_ns`, and `write_amp_milli=` per
     namespace line (`undefined` when a namespace admitted no user byte).
  2. **`AddressSpace::next_flush_chunk` chunks are additionally bounded
     by the RAM ring top** (ADR-0056 D3's contract had hole start, seal
     cut, and ro-boundary). A record ending *exactly* on the ring top
     creates no seal hole (ADR-0052 D2 seals only a record that would
     straddle), so nothing marked the wrap and a chunk could span it —
     `bytes()` returns one contiguous slice out of a wrapping ring
     (panic in a ring-sized region; wrong-end-of-ring bytes into a tier
     file under a valid CRC with region slack). The bound is free: the
     ring top is a record boundary in exactly that case, and the next
     call resumes there. Pinned by
     `flush_chunks_stop_at_an_exactly_filled_ring_top`.
- **M4-S17 amendment (ADR-0061) — blob extents: a second on-disk
  artifact class, one ordering rule, one new record tag, one new `.ick`
  section.**
  1. **Extent files** (`inf_log::blob`): one extent = one
     `blob-NNNNNN.iblob` in the per-cell `cold/` dir — 4 KiB header
     {`IBX0`, version u32 = 1, cell u32, ns u32, extent_id u64,
     data_len u64, header_crc} + tier-discipline CRC frames
     (4092 + 4), **no footer**: the referencing WAL frame's
     group-commit ack is the extent's commit record. `ExtentWriter`
     (chunked appends, 256-frame batched device writes, staging bounded
     by one batch window + one tail frame) → `finish()` fdatasyncs and
     builds a `SealedExtent`, and `finish_deferred` builds one before any
     sync is issued (its caller owes the barrier).
     `TieredTable::insert_extent`/`update_extent` take the token;
     `MutationEffect::StringSetExtent` holds a namespace, the key and
     three integers, and nothing in its type ties it to a
     `SealedExtent`, so "extent durable before referencing ack" is kept
     by the LOG step's extent-seal hold, not by the type (the reactor
     tier's coverage-neutral `GroupCommit` ledger barrier
     is command wiring's named obligation). Extent **fsync failure is a
     typed abort** (extent abandoned, id quarantined, never retried —
     the one ADR-audited posture narrower than fsync fail-stop; the module is
     allowlisted in `check-fsync-fail-stop.sh`); fault points
     `blob_short_write` / `blob_fsync_err` / `blob_unlink_fail`
     (non-fatal; absent-is-success — a replayed death legitimately
     re-offers a prior-life unlink). Reads: `ExtentReader::read`
     verifies-and-**appends** per frame (streamed chunks compose; one
     window of resident staging). Decoder: `parse_extent_header` +
     `inspect_extent_bytes`, fuzzed by `extent_decode`.
  2. **Record vocabulary**: store `TypeTag::StringExtent = 3` — value
     bytes are the 24-byte `ExtentRef {extent_id u64, offset u64 (0 in
     v1), len u64}`; WAL/image `RecordType::StringExtentRef = 9` (no
     version field — the `StringPostImage` rule). Resident extent
     records image as tag-9 records in ordinary 0x01 sections;
     references move verbatim through WAL, flush, checkpoint, and
     compaction relocation — blob bytes never flow through any of them.
  3. **`.ick` v2 tag 0x05 — blob-reference section** (registry
     re-coordinated: M4.5 index sidecars move to 0x06+): `ns u32 ·
     count × (addr u48 LE · extent_id u64 · len u64)`, entries strictly
     ascending by address, `len > 0`, never empty — the reference map's
     cold entries, emitted at walk end per tiered namespace. Decode
     audits shape/order/lengths fail-stop; records-only and no-arm
     loaders refuse typed (`BlobRefSectionUnsupported`). The hybrid
     reader (`next_step_hybrid`/`read_ick_hybrid`) grew a fourth
     handler; `apply_blob_ref_section` joins the recovery appliers.
     Counted in `records_total`, excluded from per-ns presize hints (a
     cold blob record's slot was counted by its 0x03 ref).
  4. **Reclaim gates on death durability, never checkpoints** (the
     asymmetry: deaths are logged, relocations are not) — refcount 0 ∧
     killing record's staging epoch ≤ the plane-supplied durable epoch
     ∧ plane read pins; extents therefore **never join the MANIFEST**
     (ADR-0057 D5's anticipated blob clause resolved to no schema
     change), and the tier boot-GC's "unmanifested ⇒ garbage" rule
     deliberately does not extend to `.iblob` — liveness is the
     post-replay refcounts (`recover_tiered_ns` lists extent names in
     its existing directory pass; `RecoveredTier::extents_listed` seeds
     `TieredTable::extent_sweep_seed`, drained by MAINTAIN slices).
     A park latched at a transient replay zero **revokes at
     re-registration** (`ExtentRefs::register`) — the ADR-0057 D4
     at-least-once physics applied to the reclaim queue (found by the
     DST sweep; ADR-0061 D5).
- **Accepted 2026-09-22, implementation open — ADR-0148:** bounded tiered
  namespace lifetimes amends the namespace DDL contract below. The catalog
  writer reserves a tiered lifetime grant before publication;
  `CreateApplied` does not return it. Durable removal plus every cell's
  terminal-cleanup receipt permits reuse. A node admits at most 64 tiered
  namespace lifetimes (`TIERED_NAMESPACE_LIFETIMES_PER_NODE_MAX`;
  reserved, declared and retiring alike). At capacity a tiered `CREATE`
  reserves nothing and answers `-TRYAGAIN tiered namespace capacity in
  use` while a grant is retiring, else `-ERR tiered namespace limit
  reached`. Namespace IDs never repeat: allocation answers `-ERR namespace
  identity space exhausted` before the `u32` counter wraps. Boot refuses a
  catalog with more than 64 live tiered definitions with a typed startup
  error and serves none of it. After `META` publishes a create, an origin
  that fails to construct rolls back as a refused peer does (durable
  withdrawal, a `DROP` fan, the grant held to terminal cleanup), and a
  disconnect after the persist request cannot abandon the create (ADR-0148
  D1, D4, D5, A1; ADR-0172 D3). The existing nine-argument fan and durable
  formats remain. These changes are not built; the descriptions below are
  the implemented contract.
- **ADR-0103 amendment (2026-09-01) — the `CREATE` choreography:
  persist-then-serve.**
  1. **Order** (`inf-server::plane::program_ns_ddl`): *parse → durable-
     plane check → allocate the id → `Keyspace::ns_create_check`
     (registry rules + tier gauntlet + reserved-VA arithmetic, applies
     nothing) → `ControlHandle::request_persist_create(export ∪ spec)`
     → wait `persisted(epoch)` → read the `CreateVerdict` → apply
     locally → fan `INF.NSFAN CREATE` (unchanged 9 args) →
     `create_applied(id)` → `+OK`*. No cell names a namespace before
     `META` does. `SET` keeps *apply → fan → persist → ack* (until
     ADR-0189 builds its order, below). `DROP`'s order is drawn once, at
     `INF.NSFAN DROP` below.
  2. **The writer's pending-create set** (`inf-server::control`,
     ADR-0103 D2): every `PersistReq` may carry `create: (NsSpec,
     CreateVerdict)`; the writer merges every pending spec the payload
     lacks (by id) *before* the tombstone reconciliation, decides the
     create against the merged view (`Accepted | NameExists |
     AtCapacity`, written to the request's `Arc<AtomicU8>` before
     `persisted_epoch` publishes), and retires an entry on
     `CreateApplied { id }` or on any `DROP` of the id
     (`request_persist_drop(catalog, id, tombstone)`). Bound
     `PENDING_CREATE_MAX = 256` (gauge
     `ControlHandle::pending_creates`; the origin answers `BUSY` at the cap). Empty at boot —
     `META` already holds every accepted create. The sim's inline
     inbox runs the same state.
  3. **Recovery verifier** (`inf-store::keyspace`,
     `inf-server::recover`, ADR-0103 D4):
     `ReplayOutcome::SkippedUnknownNs(NsId)` carries the id;
     `RecoverStats::records_skipped_unknown_ns` counts the skips whose id
     no drop tombstone explains (the `RecoveryBoard` slot exposes it; the
     boot line prints it when nonzero). Zero on every honest boot — the DST
     asserts it.
  4. **Fault point** `ns_create_after_meta` (crash-matrix row
     `namespace-seeded-from-meta`): the restart seeds the namespace
     from `META` and serves it.
- **ADR-0108 amendment (2026-09-02) — one DDL program at a time; a failed
  `CREATE` rolls back.**
  1. **The DDL ticket** (`inf-server::control::DdlTicket`): every
     `INF.NS CREATE`/`SET`/`DROP` program takes the node-wide ticket
     (`ControlHandle::ddl_try_acquire(cell)`, CAS on `ddl_holder`)
     before its first effect and holds it — a guard, dropped on every
     exit path — until its reply; a program that finds it held parks
     on the DDL waitlist and retries on the release edge MAINTAIN
     detects (`ddl_generation`, one relaxed load per iteration, the
     persisted-epoch pattern). Fan legs of one program never cross
     another's.
  2. **Every fan leg answers** (`plane::fan_all_or_first_error`): a
     `CREATE`/`SET`/`DROP` fan awaits every peer; a leg that cannot be
     sent or answers no bytes is an error leg.
  3. **`CREATE` rollback** (`plane::rollback_create`, under the same
     ticket): on any error leg the origin drops its copy, requests the
     catalog persist that drops the id (`request_persist_drop` — the
     pending entry retires, a durable namespace gets its tombstone),
     waits the swap, fans `INF.NSFAN DROP name epoch` (a peer without
     the namespace answers "not found", accepted), stamps a durable
     drop for retirement, and answers the leg's error (until ADR-0189
     builds its order, below). The order
     becomes *… → apply → fan → (all `+OK` → `create_applied` → `+OK` |
     any error → rollback → the error)*.
  4. **Fault point** `ns_create_fan_refused` (`plane::handle_ns_apply`,
     a peer's `CREATE` leg before its local apply; crash-matrix row
     `create-rolled-back`): no cell serves the namespace before or
     after a restart. DST `m2-ns-ddl-race` (two crossing rounds with a
     frozen origin, the refused-leg round, cuts and audits).
  5. **Connection-level commands on any binding**
     (`inf_wire::keyspace_scope`, `plane::keyspace_level`): a command with
     no key position that is not a scatter program (`PING`, `ECHO`,
     `HELLO`, `QUIT`, `CLIENT`, `COMMAND`, `LOLWUT`, `DEBUG SLEEP`, …)
     executes through the ordinary path on a tiered-bound connection — on
     the connection's cell and on the `ApplyNs` owner side alike — and the
     planeless `execute` refuses a tiered namespace only for a command
     that addresses it.
- **ADR-0102 amendment (2026-09-01) — tier ring invariants at the
  gauntlet.**
  1. **`TierSpec::validate`** gains the four-page floor (`MEM-BUDGET +
     MAINTAIN-SLICE ≥ 4mb`, `RING_WINDOW_MIN_BYTES`) and the half-ring
     inline bound (`blob_threshold_bytes ≤
     TierSpec::blob_threshold_max(R)` = `R/2 − HEADER_LEN − MAX_KEY_LEN + 1`, `R` derived from
     the spec). `for_budget` and the `INF.NS CREATE` parser derive an
     absent `BLOB-THRESHOLD` as `min(16mb, R/4)`
     (`TierSpec::with_default_blob_threshold`); an explicit value is honoured or
     refused, never clamped.
  2. **`TieredTable`** enforces the bound structurally: the effective
     `BlobConfig` threshold is clamped to `blob_threshold_max(R)` at
     construction and `set_blob_config`; `append`/`append_extent`
     refuse a record above `inline_record_max() = R/2` with
     `OpError::TooLarge`; `write_stall_target` is total (`None` above
     the bound) and `extent_stall_target(key)` sizes the blob path's
     probe from the 24-byte reference (`plane::tiered::StallProbe`).
     `AddressSpace::{new, alloc, stall_target}` keep their asserts as
     internal invariants.
  3. **Legacy catalogs**: `Keyspace::seed_catalog` clamps a pre-rule
     entry's threshold to the bound (`seed_normalized_thresholds`,
     reported by the boot).
  4. **`INFO tiering`** gains `tiering_write_replans` (writes that
     re-resolved because the slot moved under an extent read).
  5. **`inf-runtime::cold`**: an intent joins a merged read only on
     the same descriptor (`file` *and* `fd`).
- **ADR-0100 amendment (2026-09-01) — namespace catalog v4 + the `DROP`
  choreography.**
  1. **Catalog v4** (`inf-store::catalog`): after the (now always
     present) index section, `dropped_count: u32 · dropped_id: u32 *
     count` — strictly ascending ids, each `≥ 16`, `< next_id`, never a
     live entry, `≤ DROPPED_IDS_MAX` (4096); every violation is the
     typed `InvalidDroppedId`. Written **iff non-empty** (else v3/v2
     byte-identically); v1–v3 decode with an empty set. The set is
     **owned by the catalog writer** (`inf-server::control`, ADR-0100
     D2): cell exports leave it empty, the writer merges its live set
     into every payload and drops any entry a tombstone names (replica
     lag from a concurrent DDL — ids never reuse). A tombstone is added
     by a durable namespace's `DROP`, stamped with the node-wide
     checkpoint epoch the origin requests after the fan (the cleanup unit
     the `DROP` reserved before any effect, ADR-0159 D2), and retired at
     the next persist once every cell's published epoch covers it; boot
     re-stamps survivors with one fresh request on the control writer's
     quota. Bound `DROPPED_NS_MAX
     = 256` — at the cap the `DROP` waits for its checkpoint first.
  2. **`INF.NSFAN DROP name epoch`** (4 positional args): the fan carries
     the persist epoch of the swap that drops the namespace; peers park
     their tier-file teardown until `ControlHandle::persisted(epoch)`
     (ADR-0100 D5). `DROP` itself runs *apply → request persist → wait
     `persisted(epoch)` → fan → request a node checkpoint + `StampDrop` →
     `+OK`*: the persist is requested before the fan, and the fan carries
     its epoch (ADR-0100 D4) (until ADR-0189 builds its order, below).
     `CREATE` persists before it serves (item 1 of the `CREATE`
     choreography above) and `SET` runs *apply → fan → persist → ack*
     (ADR-0015 D3) (until ADR-0189 builds its order, below).
     `Keyspace::ns_drop` returns the dropped `NsSpec`;
     `ns_tombstoned`/`ns_tombstones` expose the boot snapshot to recovery.
  3. **Recovery rule** (`inf-server::recover`, ADR-0100 D6): a `MANIFEST`
     tier section naming an id the catalog does not know is skipped and
     its `shard-k/ns-N/cold` files unlinked **iff the id is tombstoned**
     (`RecoverStats::{dropped_ns_swept, dropped_files_swept}`); every
     tombstoned id's directory is swept even without a section; the
     checkpoint's tiered sections (ref / live-set / blob-ref) naming a
     tombstoned id are skipped (`dropped_sections_skipped`);
     an untombstoned unknown id stays the fail-stop in both places. `INFO persistence`
     gains `ns_drop_tombstones`. Fault points `ns_drop_before_meta` /
     `ns_drop_after_meta` carry the two crash-matrix rows.

> **ADR-0189 (2026-10-10):** the namespace DDL and drop contract once
> accepted, restating ADR-0100 whole: the catalog writer answers each
> request in queue order after its `META` write, and a refusal writes
> nothing; every namespace DDL persists before any cell applies it; a
> durable namespace's tombstone retires once every cell has published past
> its stamp and reported its `ns-N/` gone; at 256 tombstones the writer
> alone admits, paces once or refuses. Not built.

- **M4-S19 amendment (ADR-0062) — namespace catalog v2 + the `INF.NS`
  tiering surface.**
  1. **Catalog v2** (`inf-store::catalog`): `CATALOG_VERSION = 2` — the
     entry gains `tier: u8` (0|1) after `maxmemory`, followed (when 1)
     by the fixed 46-byte tier block `{mem_budget u64, disk_budget u64,
     mutable_permille u16, maintain_slice u64, cold_read_qd u16,
     compaction_dead_ratio u8, compaction_slice u64, blob_threshold
     u32, tier_io_mode u8 (0 buffered, 1 direct), tail_stall_timeout_ms
     u32}`; `name_len · name` stays last. **v1 decodes forever** (the
     v0.3.0-alpha upgrade path — every entry tier-absent); versions > 2
     fail-stop; the strict truncation/trailing/invalid-byte taxonomy
     extends over the block, and a decoded block re-runs
     `TierSpec::validate` (one range gauntlet — parse, registry, and
     decode cannot drift). The decoder keeps the M2-S08 posture: the
     exhaustive strict-decode unit matrix behind `inf-log`'s
     CRC-checked `META` envelope (no fuzz target — the standing
     precedent, disclosed).
  2. **`INF.NSFAN` v2**: `CREATE` grows to 9 positional args (the 9th =
     `-` or the ten tier fields colon-joined, decoder re-validating);
     new verb `SET name tier-tuple` — `INF.NS SET` rides the same DDL
     program (fan AllOk, catalog persist-then-ack). `is_ns_ddl_sub`
     gates {CREATE, DROP, SET} into `program_ns_ddl` on every node
     shape.
  3. **Admission + teardown** (`inf-store::keyspace`):
     `materialize_tiered` is fallible-typed (`TieredCreateError` —
     `Exists | Unrepresentable | VaLimitExceeded{...}`), checked against
     the per-cell share of `tiered-reserved-va-limit` (CONFIG, Memory
     kind, HotPerCell; default 256 GiB) **before** any mmap;
     `ns_create` registers + materializes together or rolls back;
     `ns_drop` prunes `tiered_stores` (the `Region` unmaps — reserved
     VA returns structurally); `seed_catalog` re-materializes tiered
     entries through the same `ns_create` path. `ns_set_tier` +
     `TieredTable::set_demotion` (ring-bounded — growth past the
     reservation refuses typed) carry the D3 hot-reload;
     `TierSpec::{demotion,compaction,blob}_config` derive every table
     config from one validated block.

---

## Appendix A — Durable-path invariant inventory (M2.5-S13)

The written inventory INFINITY_STYLE §Assertions requires (*"Every state
machine keeps a written invariant inventory — what holds, how it is
enforced, what is deliberately unchecked"*). The **promote**/**add**
dispositions below landed with M2.5-S13 (all five promotions are
per-batch/per-checkpoint — the ≤ 1% per-op A/B was not triggered);
enforcement columns describe the tree *before* that landing.
Enforcement vocabulary and the assertion/panic policy are
INFINITY_STYLE §Safety → **Assertions** (`debug_assert!` default; release
`assert!` is a deliberate act for invariants whose violation endangers
durable state, ≤ 1% A/B — the M2.5-S13 rule) and **Panics and errors**
(panics for violated internal invariants only; operating errors return typed
errors; `expect()` with an invariant justification *is* an assertion and is
judged as one).

All file:line references are against the tree audited on 2026-07-07
(read-only). `by-construction` = the type system / ownership / control flow
makes the violation unrepresentable; `checked-error` = a typed
`Result`/branch handles it (operating error, not an assertion);
`UNCHECKED — gap` = no runtime guard, relies on caller discipline.

Legend for disposition: **keep** · **promote** (debug→release) · **add**
(new debug_assert for a gap) · **document** (deliberate gap, leave as-is with
a comment).

---

## 1. `GroupCommit` — submission-ordered fsync ledger (`inf-log/src/commit.rs`)

The watermark-honesty core: the watermark advances only through the
**done-prefix** of a submission-ordered ledger whose coverage is monotone.
Every durable ack (`WatermarkGate`) and every MANIFEST publication gates on
this value, so a coverage inversion or a premature advance is **silent
durable-state corruption** (an ack for a byte that was never fsync-covered).

| # | Invariant (identifiers) | Why it must hold | Current enforcement | Disposition |
|---|---|---|---|---|
| 1.1 | `push_pending`: `pending.back().covers_up_to <= covers_up_to` — ledger coverage monotone in submission order | A non-monotone entry lets the done-prefix advance the watermark past an earlier, less-covered (or unfsynced) entry → ack before durable | `debug_assert!` commit.rs:498 | **promote** — this is *the* watermark-honesty invariant; per-batch, free |
| 1.2 | `note_frame_queued`: `queued_up_to < end` — frames queue in strictly increasing LSN order | Out-of-order queue breaks the LSN↔seq FIFO the ack gate and reader rely on | `debug_assert!` commit.rs:408 | **promote** — per-batch, free |
| 1.3 | `note_frame_queued` with a barrier: every owed `always` record rides the frame the barrier covers before the barrier discharges the due | A linked sync clearing `always_pending` while an `always` record is still unqueued → its ack gates on a sync that does not cover it (S06 oracle violation) | type: one call queues the frame that absorbs every staged record and registers its barrier, nothing between (ADR-0197 D2) | keep — **restated 2026-10-30:** the call's shape in place of the release assert |
| 1.4 | `register_seal_fsync`: `queued_bytes == written_bytes` — rotation only with no write in flight (seal covers a complete segment) | A seal fsync registered mid-write claims coverage of bytes not yet on the old segment | `assert_eq!` **release** commit.rs:340 | keep |
| 1.5 | `register_boot_barrier`: `frames_queued == 0` **and** `durable_up_to.is_none()` — barriers register at the ledger head before any durable traffic | A barrier behind real data would (a) not fence acks and (b) risk covering data with a zero-byte floor | two `assert!`/`assert_eq!` **release** commit.rs:372, 373 | keep (per-boot) |
| 1.6 | `on_fsync_complete`: ticket exists and `!done && !failed` — completions are exactly-once (driver contract) | A double-completion or unknown ticket corrupts the done-prefix bookkeeping | `expect` + `assert!` **release** commit.rs:538, 539 | keep |
| 1.7 | `on_fsync_error`: ticket exists and `!done` | An error on an already-done entry means the ledger and driver disagree | `expect` + `assert!` **release** commit.rs:570, 571 | keep |
| 1.8 | done-prefix advance: watermark only moves through contiguous `done && !failed` front entries | A failed/pending entry must freeze everything behind it (fsyncgate) | by-construction (loop breaks on `!done \|\| failed`) commit.rs:549–556 | keep |
| 1.9 | `register_completion_fsync`: `always_discharged_at_written()` before issuing | A completion-issued sync that does not cover an owed `always` record acks it early | `debug_assert!` commit.rs:477 (guarded by `completion_fsync_due` at call site) | keep — call-site predicate is the real guard; debug_assert is the pair |
| 1.10 | `with_sync_pipeline`: `bound ∈ 1..=2` — the pipeline is bounded, never a queue (L3) | An unbounded in-flight sync count is the batch=1.0 disease reborn | `assert!` **release** commit.rs:230 | keep (per-construction) |
| 1.11 | `note_everysec_tick`: idle tick (clean + no `always` + **nothing staged**) issues no sync — a record still in the staging builder (a held frame: fill, group hold, extent barrier, `NextNotReady`) is dirty (`everysec_unqueued`, cleared by `note_frame_queued`; ADR-0013 D3 second amendment) | A sync on a clean tick is wasted device work, not a correctness bug | by-construction (branch) commit.rs:267 | keep |
| 1.12 | `register_prealloc_barrier`: coverage-neutral — enters at the current coverage tail, never advances past real data | A dir sync that claimed data coverage would advance the watermark past unfsynced frames | by-construction (copies tail `covers_*`) commit.rs:386–394; pinned by `prealloc_barrier_is_coverage_neutral` test | keep |

**Verdict:** exceptionally well-asserted machine (32 `assert!` / 34
`assert_eq!` / 4 `debug_assert!`). The three durable-honesty invariants that
are still debug-only (1.1–1.3) are the promotion headline — all per-batch,
so promotion is free.

**M4.5-S35 additions (ADR-0087 D2/D3/D4, 2026-08-21):**

| # | Invariant (identifiers) | Why it must hold | Enforcement | Disposition |
|---|---|---|---|---|
| 1.13 | `note_frame_written(id)`: `written_up_to`/`written_bytes` advance only over the contiguous written prefix of the queued-frame FIFO; a later frame landing first advances nothing | A standalone/completion fdatasync covers `written_up_to`; claiming a frame whose write has not completed is ack-before-durable | by-construction (prefix loop) + `expect`/`assert!` **release** on unknown/double ids; pinned by `written_prefix_is_completion_ordered` | keep |
| 1.14 | `note_frame_queued` with a linked plan: no earlier write in flight (`drained()`, held at `frame_plan`) | `IO_LINK` orders the sync after *this* frame's write only; an earlier frame still in flight sits outside the sync's coverage | `assert!` **release** before the frame is queued (per linked frame, free), a claim on the plan made in the same LOG step + the `Wait` arm of `frame_plan` | keep — the 1.4 rule generalized from rotation to every linked sync; **restated 2026-10-30** (ADR-0197 D2) |
| 1.15 | `register_seal_fsync`: `drained()` — rotation is a pipeline drain point | 1.4 restated for K > 1: the plane holds a frame needing rotation until no write is in flight | `assert!` **release**; plane predicate `rotation_due && !staging.drained() ⇒ hold`; pinned by the K = 4 reference-plane test (seal syncs == rotations) | keep |
| 1.16 | `note_frame_queued`: `unwritten < MAX_FRAMES_IN_FLIGHT` (the in-flight count the ring bounds) **and** the frame queue — a `CappedDeque` of `REORDER_WINDOW_FRAMES` (16) entries, the unwritten frames plus those that landed ahead of an earlier one still in flight — has room | An unbounded in-flight count is a queue (L3) and a plane bug; a FIFO that grows behind one late front write is an unbounded ledger (memory, a linear completion search, eventually the cell) | `assert!` **release** on the in-flight count; the queue's own admission: `frame_plan` asks it (a reserve whose slot is dropped) and answers `Wait(Hold::ReorderWindow)` on `Full`, the plane counts `frame_waits_reorder`, and the queueing's `Full` arm is a release fail-stop; frames are found by ordinal arithmetic (`id − front.id`), O(1); pinned by `late_front_write_fills_the_reorder_window_then_the_plan_waits` and the `m2-reorder-window` DST, whose cut reads the `frame-queue` census row at its cap on every cell that held a frame for it | keep — **amended 2026-10-29:** the window is the queue's own capacity, so the plan and the queue read one compare |
| 1.19 | `register_standalone_fsync` / `register_completion_fsync`: the due survives a sync whose coverage (`written_bytes`) is below `queued_bytes` — a barrier-less frame still in flight at the tick is covered within the tick, not at the next one — **or while an `everysec` record is still staged** (records in the builder sit outside any written coverage; the frame that seals them carries the barrier) | An everysec record whose frame was in flight when the tick's standalone issued would otherwise wait a full extra tick (~2 s loss window; `m2-reorder-window` seed `0x2e0d0179`) | by-construction (`settle_due_at_written`); the next LOG step drains and covers (linked sync or a second standalone); pinned by `everysec_due_survives_a_standalone_that_leaves_a_frame_in_flight` | keep — **added 2026-08-22 (ADR-0013 D3 as amended)** |
| 1.17 | `note_frame_queued` with a write-through plan: coverage tail == this frame's base (the prefix rule, ADR-0086 D2.5), the base `queued_bytes` read before the frame is queued | A FUA ticket claiming bytes it did not write | `assert_eq!` **release**, a claim on the plan made in the same LOG step | keep (S34's assert; **restated 2026-10-30:** the base before the queueing, ADR-0197 D2) |
| 1.18 | `frame_plan`: a due frame that cannot take write-through while writes are in flight below is **held** (`Wait`), never sealed barrier-less | Sealing it plain lets every later frame find the same shape — the due starves under load (livelock) | by-construction (decision order) ; pinned by `a_due_frame_waits_behind_in_flight_plain_frames_then_links`; `frame_waits_barrier` counts episodes | keep |
| 1.20 | `pending_covers_within(lo, hi)` / `written_up_to()` / `pending_entries()`: the plane's ack map (`DurableCell::ack_map`, a `CappedDeque` of `ACK_MAP_ENTRIES` = 16 + 44 + 1 points) coalesces written points across which no unfolded ledger coverage point lies — every later barrier covers both or neither — at each `LogWritten`, so it holds at most `frames_behind_prefix() + pending_entries() + 1` | One entry per sealed frame accumulated for the whole life of a stalled barrier and between two ticks (L3) | the map's cap: the LOG step asks its admission before the frame's plan and holds the frame on `Full` (`frame_waits_ack_map`, derived never to occur and read 0 by the DST's cut on every durable scenario), the publish's `Full` arm a release fail-stop; `debug_assert!` of the relation in `coalesce_ack_points`; the DST's `frames_awaiting_watermark` gauge against `REORDER_WINDOW_FRAMES + fsync_entries + 1`; `ack_map_tests` pin the merge rule (a coverage point between two written frames keeps the earlier; an unwritten frame is never merged into) and the cap's edge (60, 61 admitted, 62 refused) | keep — **amended 2026-10-29:** a capped deque in place of the release assertion |
| 1.21 | `frame_plan(_, seal_ahead = true)`: the FLUSH-slot arm counts the seal fdatasync the rotation registers ahead of the frame — `syncs_in_flight() + 1 < flush_bound`, so at bound 1 the frame seals `Plain` and its due accumulates behind the seal | Two FLUSH-class barriers in one iteration queue on the device-wide flush unit (ADR-0086 Context: ~1 000/s) — the batch=1.0 disease at the device tier; no durability consequence | by-construction (the arm); `assert!` **release** in `push_pending` for `Linked`/`Standalone`/`Completion` (`FLUSH-class barrier into a full pipeline`); pinned by `rotation_seal_and_linked_sync_respect_the_flush_bound` and `a_discretionary_barrier_never_enters_a_full_pipeline`; the `m2-durable` DST dies on the assert with the plan unfixed | keep — **added 2026-09-13 (ADR-0087 D3/D4 amendment)** |
| 1.23 | `note_frame_queued` with a write-through plan: the write-through class holds at most `WRITE_THROUGH_WINDOW_ENTRIES` (16) tickets registered and not yet folded into the durable prefix | Behind a wedged FLUSH-class front (seal, dir barrier, linked sync) every FUA frame completes at `LogWritten` and its ticket stays in the ledger until the front folds — one entry per frame, bounded only by the gated clients' outstanding commands; the reorder window does not reach it (a FUA frame is written the moment its ticket completes) | 1.26's class assertion; `frame_plan` declines the write-through arm while the class is at its budget — the frame seals `Plain`, the due accumulates, the first barrier after the front covers it (a linked sync: `write_through_due` is false over an uncovered plain frame); `DurableStats::write_through_entries` under the DST's gauge oracle on every durable scenario, its window reached by `m2-ledger-wedge`; pinned by `write_through_tickets_behind_a_wedged_front_are_bounded` | keep — **amended 2026-10-29:** the window is the ledger's write-through class |
| 1.24 | `register_extent`: the extent-seal window holds one entry per unfolded `ExtentSeal` ledger entry, in ledger order, and at most `EXTENT_SEAL_WINDOW_ENTRIES` (16) | Each extent barrier holds its open file until the fold; unbounded, one iteration's burst held one descriptor per blob write until a create failed on the descriptor limit and its client read an error | the capped deque's reservation, taken before any effect (a full window runs none); `debug_assert!` of each folded ticket against the window's front; pinned by `seventeen_blob_writes_register_sixteen_extent_barriers` and `window_units_return_at_the_fold_in_ledger_order`; the `m4-extent-window` DST's sim-disk peak of open extent files ≤ 16 and its `extent-window` census row | keep |
| 1.25 | `ExtentWaiters`: `len + w ≤ 16` — every turn has a unit; an arrival reserves only while the window is open, no writer is parked and, with turns out, a unit is free beyond theirs | A refused turn parks a woken writer again behind later arrivals: the FIFO broken, a reply past its turnover | `admit`'s table test; a refused turn is a `debug_assert!` and, in release, a counted re-park (`turn_refused`, read 0 by the DST's census oracle on every durable scenario) | keep |
| 1.26 | `push_pending`: every class of the fsync ledger holds at most its budget unfolded — boot 5, the rotation's seal 1, prealloc 2 and zero-fill 2, extent 16, write-through 16, discretionary 2 — and the budgets sum to the ledger's cap, `FSYNC_LEDGER_ENTRIES` (44) | The ledger took seal, prealloc, zero-fill and boot entries outside every bound: behind one undone front each rotation added a seal and two barriers, and at a FLUSH bound of 2 each finished linked sync one more unfolded entry | `assert!` **release** at the registration for every class the ledger counts (`a ledger class past its budget`; the extent class is bounded by its window's own admission, which a second check would hide from the census and the disk that judge it), on a plan made in the same step: `frame_plan` holds a rotation while a seal is unfolded (`Wait(Hold::SealUnfolded)`, counted in `frame_waits_ledger`) and declines write-through and linked syncs at their budgets, `standalone_fsync_due` and `completion_fsync_due` read the discretionary class, MAINTAIN skips a prealloc or zero-fill at its budget (`ledger_prealloc_deferrals`, derived never to occur), `BootDirs` holds at most four directory handles, and the extent class is its window; a const assertion ties the budgets to the cap; pinned by `a_rotation_holds_until_the_previous_seal_folds`, `a_seal_registered_past_its_class_budget_panics`, `every_class_fills_to_its_budget_and_the_ledger_holds_them_all` and `the_ledger_cap_admits_every_class_at_its_budget`, and by the `m2-ledger-wedge` DST's disk clause (at most one segment id appears while a seal is held) | keep — **added 2026-10-29** |
| 1.27 | `note_frame_queued` / `note_frame_written`: a queue entry holds its frame's staging lease, submit time and the barrier its write settles until its one `LogWritten`, which hands them back | A second table beside the queue (the plane's in-flight list, found by a scan and removed mid-list) is a second fact of which frames are unwritten | type: `QueuedFrame::flight` is `Unwritten { lease, submitted_at, barrier }` or `Written`; a second `LogWritten` is a release fail-stop (`frame written twice`); the call that publishes the entry registers its barrier, once, with the flight whole (ADR-0197 D2) | keep — **added 2026-10-29**, **restated 2026-10-30** |
| 1.22 | `DurableCell::note_issued`: a hold episode (`frame_held`, `fill_since`, `group_since`) ends only at the issue — the frame's `queue_frame` or the standalone's fdatasync; a reservation that waits keeps the episode | The standalone path left `group_since` open, so the next episode's first sight read as elapsed and sealed at once (the hold intermittently inert, `frame_waits_group` under-counted); the reservation-failure returns inflated `frame_waits_*` by one per return | two `assert!` **release** at each decision (`open group-hold episode without a held frame`, the fill twin); `DurableStats::{hold_open, fill_hold_open, group_hold_open}` under the DST's hold-episode oracle on every durable scenario (`m2-group-hold` 24/24 seeds red pre-fix, `m2-durable` 6/24 — the ARM seeds) | keep — **added 2026-09-13 (ADR-0092 D1 rule 6 amendment)** |

---

## 2. `WatermarkGate` — LSN-keyed ack gate (`inf-runtime/src/gate.rs`)

> **Accepted 2026-10-19, implementation open — ADR-0177:** D5 rebuilds this
> gate as an indexed minimum heap over the cell's wait-node pool. A pending
> `always` reply carries its record's LSN, and its task waits on the gate
> for one LSN at a time, where the group-commit section above has every
> `always` future register. `waiter(lsn)` becomes `wait_for(lsn)`: at or
> below the watermark it is Ready at its first poll and takes no node
> (row 2.3). A dropped wait leaves the heap in the same step, so row 2.4's
> cancelled state and its `unreachable!` go; an `always` reply's wait is
> still dropped only with its task. Rows 2.1, 2.2 and 2.5 are unchanged.
> The rows below do not establish that the replacement is built.

Wakes `always` response futures when the durability watermark reaches their
record LSN. It *trusts* the LSN fed by `GroupCommit` (machine 1); its own
job is monotonicity + correct FIFO wake.

| # | Invariant | Why it must hold | Current enforcement | Disposition |
|---|---|---|---|---|
| 2.1 | `advance`: watermark is monotone; `to <= current` is a no-op | A regressing watermark would un-ack or double-wake | by-construction (early return) gate.rs:403 | keep, but see 2.2 |
| 2.2 | `advance` **silently** ignores `to < current` (a caller feeding a lower watermark) | A `GroupCommit`↔gate wiring bug (feeding a stale watermark) is masked as a no-op instead of caught | **UNCHECKED — gap** (deliberate: doc says "lower values are no-ops") | **document** — the no-op is intended for idempotent re-advances; a `debug_assert!(to >= watermark || <known re-advance>)` is not cleanly expressible because equal/replayed advances are legal. Leave as deliberate gap; note in inventory |
| 2.3 | `waiter(lsn)`: a waiter at or below the current watermark is `Woken` at construction (same-iteration delivery) | A missed same-iteration wake parks a future forever | by-construction gate.rs:387 | keep |
| 2.4 | `WatermarkWait` is never cancelled (unlike `WaitList`) | An `always` ack future outlives to completion; cancellation would drop an ack | `unreachable!` gate.rs:468 (panics in release too) | keep |
| 2.5 | single writer (owning cell thread; no atomics) | L1: shared mutable data-plane state is forbidden | by-construction (`Rc`/`Cell`/`RefCell`, cell-resident) | keep |

**Verdict:** correct by construction; the only gap (2.2) is a deliberate
idempotence affordance, documented, not a promotion target.

---

## 3. `StagingRing` custody — log-staging domain (`inf-log/src/staging.rs`)

> **ADR-0191 (2026-10-29), implementation open:** rows 3.5 and 3.9 reverse
> their "keep (per-boot)". The bounds they name —
> `capacity_bytes ∈ [MIN_FRAME_LEN, DEFAULT_MAX_FRAME_LEN]` and
> `frames_in_flight ∈ 1..=MAX_FRAMES_IN_FLIGHT` — are rows 1–3 of
> `LogGeometry::new`, checked once into the type the ring is built from, and
> `StagingRing::new` asserts neither: a pair outside them is a
> `LogGeometryError` at construction, never a panic. Row 3.9's allocation
> facts — buffers = K + 1 allocated once, `resident_bytes` = (K + 1) ×
> (capacity + 2 × FRAME_ALIGN) — hold by construction, as before. Rows 3.1–3.4
> and 3.6–3.8 are unchanged. The rows below do not establish that the
> replacement is built.

Double-buffered frame pair; the **generation token** is the custody
mechanism (a stale `StagedAt`/`FrameLease` resolving to the wrong LSN would
mis-address a record → wrong data acked as durable).

| # | Invariant | Why it must hold | Current enforcement | Disposition |
|---|---|---|---|---|
| 3.1 | `lsn_of`: `at.generation == self.generation` — token belongs to this lease | A cross-generation token resolves a record to a bogus LSN → an ack gates on the wrong watermark | `assert_eq!` **release** staging.rs:138 | keep (the custody invariant; already release — exemplary) |
| 3.2 | `leased_frame`: `in_flight.generation == lease.generation` | Writing the wrong buffer's bytes to the segment | `assert_eq!` **release** staging.rs:349 | keep |
| 3.3 | `release`: `in_flight.generation == lease.generation` | Releasing the wrong buffer resurrects in-flight bytes | `assert_eq!` **release** staging.rs:357 | keep |
| 3.4 | `seal`: `!is_empty()` and `in_flight.is_none()` — at most one frame in flight, no empty seal | A second seal while a lease is outstanding aliases the in-flight buffer (memory > 2×capacity, torn write) | two `assert!` **release** staging.rs:328, 329 | keep — **amended M4.5-S35 (ADR-0087 D1):** `!backlogged()` (a free buffer exists; ≤ `frames_in_flight` leases out); the seal takes a `Free` buffer, never one in `InFlight`/`Staging` (typestate per buffer) |
| 3.8 | `leased_index`/`release` (M4.5-S35): exactly one buffer is `InFlight { generation }` for a live lease; release moves it to `Free` in any order | A stale or double-released lease would free a buffer still leased to a write | `expect` **release** (position by generation) | keep |
| 3.9 | `new` (M4.5-S35): `frames_in_flight ∈ 1..=MAX_FRAMES_IN_FLIGHT`; buffers = K + 1 allocated once; `resident_bytes` = (K + 1) × (capacity + 2 × FRAME_ALIGN) | The ring is bounded and attributed (L5); no allocation after construction (pinned at K = 4 by `staging_alloc`) | two `assert!` **release** | keep (per-boot) |
| 3.5 | `new`: `capacity_bytes ∈ [min_frame, DEFAULT_MAX_FRAME_LEN]` | A capacity below a minimal frame or above the reader bound writes frames no default reader can replay | two `assert!` **release** staging.rs:204, 206 | keep (per-boot) |
| 3.6 | `stage`: record over remaining capacity → typed `StagingFull`, effect not partially staged | Backpressure is an operating condition, not an invariant (L: bounded queue) | checked-error staging.rs:227–230 | keep |
| 3.7 | oversized record (`> max_record_len`) is admission's problem, never retried here | Retrying an un-stageable record is a livelock | by-construction + doc (admission checks `max_record_len` up front) staging.rs:254–260 | keep — verify S08 admission actually calls `max_record_len` (out of scope here; note for cross-check) |

**Verdict:** the custody invariants are already release asserts — the
model chapter for this whole audit. Nothing to promote.

---

## 4. Checkpoint + MANIFEST swap (`inf-server/src/ckpt.rs`)

`CkptPhase` (stream) and `SwapPhase` (recovery-unit transition). Failure
policy here is **abort, never fail-stop** (nothing is acked against an
in-flight checkpoint) — deliberately narrower than the log path's fsync
fail-stop rule.
The dangerous invariant is publication: naming (in the durable MANIFEST) a
checkpoint whose data is not fsync-durable → recovery loads a
short/torn checkpoint = **silent durable-state corruption**.

| # | Invariant | Why it must hold | Current enforcement | Disposition |
|---|---|---|---|---|
| 4.1 | `publish`: `st.sync_done && st.in_flight.is_none()` — the `.ick` is renamed only after its completion fdatasync landed and no section write is outstanding | Renaming a not-yet-durable `.ick` into place lets a crash-after-MANIFEST recovery load a checkpoint missing its tail sections | `debug_assert!` ckpt.rs:220 | **promote** (split into two, §Split-compound) — per-checkpoint, free; **headline** |
| 4.2 | `note_published_ick`: `self.idle()` — one recovery-unit transition in flight, ever | Two concurrent swaps could publish a MANIFEST naming a unit whose begin marker is not yet watermark-covered | `debug_assert!` ckpt.rs:445 | **promote** — per-checkpoint, free |
| 4.3 | `WatermarkWait` → stage guard: `watermark >= pending.begin_lsn` before writing `MANIFEST.new` (the publication guard) | A MANIFEST naming a begin-LSN beyond the durable watermark names a recovery unit whose begin marker may not be on disk | checked-branch (`is_none_or`, re-parks) ckpt.rs:553–557 | keep — control-flow guard; consider a paired release `assert!` at the `DirSynced` commit (see promotions, optional) |
| 4.4 | `on_synced`: phase is one of the three barrier-in-flight variants | A `ManifestSync` completion with no matching phase = ledger/driver disagreement (internal invariant) | `panic!` ckpt.rs:472 (release) | keep — violated-internal-invariant, panic is policy-correct |
| 4.5 | `on_sync_error`: phase is a barrier-in-flight variant | same | `panic!` ckpt.rs:493 (release) | keep |
| 4.6 | `open_stream`/streaming: at most one `SectionLease` in flight (`in_flight: Option`) | A second section write aliases the stream buffer | by-construction (`Option` + `if in_flight.is_some() return`) durable.rs:508, ckpt.rs:76 | keep |
| 4.7 | abort leaves the old checkpoint + whole log valid; `.new` orphan GC'd next boot | Checkpoint failure must never touch the live recovery unit | by-construction + boot GC (recover.rs:706–713) ckpt.rs:239–252 | keep |
| 4.8 | `swap_slice`: every arm does O(1) file ops and queues ≤ 1 barrier; no device barrier on the loop | A blocking dir-fsync on the reactor is the S12 foreground-stall / D7-wedge class | by-construction (barriers ride `TokenClass::ManifestSync`) ckpt.rs:508–659 | keep |
| 4.9 | `derive_interval` (M4.5-S36, ADR-0088 D4): `floor ≤ interval ≤ max(cap, floor)`; `interval_bytes == 0` stays 0 (manual only) | An interval above the cap lets a replay tail escape the boot gate; below the floor is the pre-S36 trigger storm at small datasets | two `assert!` **release** (per publish, free) | keep (per-publish) |
| 4.10 | `should_begin`: fires on on-disk frame bytes **or** the record cap since the last *begin* (never publish) | A staged-bytes accumulator under-counts the device (ADR-0086 D3); rebasing at publish lets a burst mid-walk escape | by-construction (two-term predicate, begin-anchored bases) | keep |
| 4.11 | `lease_staging` (v3): the block is padded to `ICK_BLOCK_ALIGN`, its base is aligned, `file_offset` is aligned | A misaligned `O_DIRECT` write is `EINVAL` on the device and a silent memcpy on tmpfs; the sim tier asserts it on every direct inode | `debug_assert!` ×2 (writer) + **release** `assert_eq!` ×2 in `SimDisk::assert_direct_aligned` + the fdinfo verify at open | keep — the sim is the enforcement tier |
| 4.12 | `Block::realign`: after any `Vec` growth the content base is re-aligned (`debug_assert` on the new base) | A record outrunning the section target reallocates the `Vec`; an unaligned base is 4.11's failure one step earlier | `debug_assert_eq!` + by-construction (`reserve` before the shift) | keep |

**Verdict:** the two publication invariants (4.1, 4.2) that stand between a
crash and a corrupt recovery unit are debug-only. Promote both — free.

---

## 5. `Recovery` state steps (`inf-server/src/recover.rs`)

**M4.5-S35 amendment (ADR-0087 D6, amends ADR-0031 D4):** the slack audit of
segment `idx` runs immediately after its replay (`Replay{idx} → Audit{idx}`);
the first slack holding a validating frame is the **hole**, after which
every later segment is `Probe`d for stamp evidence and never replayed. `Finish`
applies the one evidence rule at the resume point (the hole's segment when it
holds data, else the last data-bearing segment before it): a v1 frame or a
`covered_lsn` attestation past the hole refuses (covered data lost — the
device lied); otherwise truncate there, remove every later segment, resume
under a fresh epoch. Invariants: `ends.len() == evidence.len() ==
segments.len()` at `Finish` (debug-asserted); `Probe` only behind a hole
(debug-asserted); the end-of-replay checks (tiered, sidecar) run exactly once
on the first transition out of `Replay`. Taxonomy pins: `recover_torn.rs`
(`unattested_hole_in_a_sealed_segment_truncates_and_discards_later_segments`,
`unattested_hole_at_a_sealed_segments_first_frame_resumes_in_the_previous_segment`,
`attested_hole_in_a_sealed_segment_refuses_to_start`), crash row
`seal-flush-x-fua-plain-tail`.

Resumable boot machine (`Phase`: Start→Ick→Replay→Audit→Finish→Complete).
Recovery is where a wrong decision **is** durable corruption made visible;
its dangerous invariants are already handled as **fail-stop checked errors**
(the correct disposition — these are operating conditions triggered by
on-disk state, so per §Panics they are typed errors, not assertions).

| # | Invariant | Why it must hold | Current enforcement | Disposition |
|---|---|---|---|---|
| 5.1 | log's valid data must not end below `begin_lsn`: `resume >= begin` | Everything the MANIFEST names was durable at publication; a shorter log = fsync-covered bytes lost (disk lying) | checked-error → fail-stop recover.rs:668–675 | keep — **this is the headline durable-honesty guard; correct as a typed error** |
| 5.2 | MANIFEST present ⇒ floor segment exists (`scan.first() == Some(floor)`) | The floor holds the begin marker and is never truncated; its absence = lost named state | checked-error recover.rs:396–404 | keep |
| 5.3 | MANIFEST-listed tail segment must not exceed the on-disk tail | A MANIFEST naming a segment the log lacks = corruption | checked-error recover.rs:405–411 | keep |
| 5.4 | `.ick` header (`ckpt_id`, `begin_lsn`, `cell`) must equal the MANIFEST | Loading a checkpoint that disagrees with its name = wrong recovery unit | checked-error recover.rs:422–437 | keep |
| 5.5 | Audit: a validating self-located frame beyond a segment's data end is unreachable interior data → fail-stop | A replay that skipped a gap and later resurrected a survivor out of order = silent corruption | checked-error (`RegionScan::ValidFrame`) recover.rs:625–639 | keep |
| 5.6 | torn-tail truncation never rewrites bytes, only the tail pointer; trailing segments verified frame-free before removal | Rewriting bytes or deleting a segment with a valid frame = data loss | by-construction + 5.5 audit gates removals recover.rs:676–692 | keep |
| 5.7 | Io read errors fail-stop above the classify arm | An Io error must not be misclassified as a torn tail | `unreachable!` recover.rs:592 (genuinely unreachable — matched above) | keep |
| 5.8 | phase-index bounds: `segments[idx]`, `ends[idx]`, `residue[last_data..]`, `seg_sizes[idx]` | An out-of-bounds index panics on boot; indices are machine-driven, not input-driven | by-construction (phase transitions bound `idx`; `residue`/`ends` get one entry per segment before Finish) recover.rs:516, 577, 599, 622–623, 653–681 | keep — **UNCHECKED but by-construction**; the vector *lengths* derive from the disk scan while the *indices* derive from phase transitions, so a scan/phase desync would panic (loud), never corrupt. Optional low-value `debug_assert!(idx < self.segments.len())` in `step_finish`; not required |
| 5.9 | `finish` only after `Complete`; `fs` present until finish; `scan`/`manifest` set by their phase | Calling out of order = machine misuse (internal invariant) | `expect` recover.rs:328, 356, 444, 471, 658, 662, 715, 716 | keep — violated-internal-invariant, justified `expect` = assertion |

**Verdict:** correctly hardened — the corruption-class invariants are
fail-stop typed errors (the right tool; an `assert!` here would be *wrong*
because on-disk state is an operating condition). No promotions.

---

## 6. `RecoveryBoard` boot barrier neighborhood (`inf-server/src/control.rs`, driven from `plane.rs`; ADR-0026)

Cross-thread control-plane observability (the L1 carve-out, ADR-0026): each
`CellRecoverySlot` is single-writer (owning cell, relaxed stores); readers
are the control narrator and `INFO`. Not a durability machine itself, but
its `phase`-before-step contract is what turned the D7 silent wedge into a
named stall, and `mark_ready` is the `-LOADING`→serving edge.

| # | Invariant | Why it must hold | Current enforcement | Disposition |
|---|---|---|---|---|
| 6.1 | phase is published **before** each recovery step | A step that stalls in the kernel must leave the board naming the stuck phase (the D7 wedge was invisible because nothing was published around the blocking section) | by-construction (call order) plane.rs:907–913 | keep |
| 6.2 | `mark_ready` uses `Release` store; `ready()` uses `Acquire` — the recovered state is visible before `ready` observes `1` | A relaxed publish could let another thread see `ready` before the cell's writes land | by-construction (Ordering) control.rs:63, 102 | keep |
| 6.3 | single writer per slot (owning cell) | Two writers = torn progress fields / a data race (L1) | **UNCHECKED — gap** (documented ownership contract, no runtime guard) control.rs:26–29 | **document** — enforced by the assembly wiring (one cell owns its slot); a runtime guard would need a writer-id and is not worth the atomic. Leave as documented deliberate gap |
| 6.4 | `mark_ready` called once per cell | A second `mark_ready` would re-narrate / mis-count records | UNCHECKED — by-construction (the `Complete` arm runs once, `boot.take()`) plane.rs:943–964 | keep — by-construction via `self.boot.take()`; optional `debug_assert!(state==0)` inside `mark_ready` |
| 6.5 | `slot(cell)` / `ckpt_board.slot(cell)` in bounds | OOB cell id = assembly bug | `panic!` (indexing) control.rs:154, 245 | keep — violated-internal-invariant (config), documented `# Panics` |
| 6.6 | boot barriers armed at `Complete` fence every durable ack (done-prefix) before ready serves `always` | A ready cell acking `always` before boot-metadata is durable would lose acked writes on an entangled-boot crash | by-construction (arm_boot_barriers at ledger head, invariant 1.5) plane.rs:955–957, durable.rs:202–215 | keep |

**Verdict:** the durability-relevant fencing (6.6) is by-construction via
machine 1. The one true gap (6.3 single-writer) is a documented L1 carve-out;
no cheap runtime guard exists. `mark_ready`-once (6.4) is a candidate for a
free `debug_assert`.

---

## Summary of dispositions

- **Promote to release `assert!` (5):** commit.rs:498 (1.1), commit.rs:408
  (1.2), commit.rs:432 (1.3), ckpt.rs:220 (4.1, split ×2), ckpt.rs:445
  (4.2). All per-batch / per-checkpoint → free (no per-op A/B needed).
- **Add debug_assert (gaps worth closing):** durable.rs:358 (ack-seq
  monotone, see panic-policy/promotions), ckpt.rs `DirSynced` optional
  paired publication-guard assert, control.rs `mark_ready`-once.
- **Document as deliberate gap (2):** gate.rs `advance` silent-no-op (2.2),
  control.rs single-writer-per-slot (6.3).
- **Keep (well-asserted, no change):** all of StagingRing (release custody
  asserts), all Recovery fail-stop typed errors, the GroupCommit
  release-asserted registrations, WatermarkGate.

Total invariants inventoried: **39** across 6 machines (+ the DurableCell
glue). Deliberate unchecked gaps: **2**. Promotions proposed: **5** (6
call sites after the split).

### A.7 — Tier-flush round machine (M4.5-S31, ADR-0084)

The reactor-drive flush state machine (`TierFlush` round state in
`inf-log`, `FlushRound` bookkeeping in `inf-server/tier_cell/round.rs`):

- **A round is planned before it stages** (ADR-0155 D1–D3): its span is
  the largest whole-record prefix at the flush cursor within the slice
  `S`, or the first record alone, chosen by the records' own headers and
  ending at a hole, the ro-boundary or the ring top — never by a recorded
  seal cut — so it is at most `max(S, M)` bytes (`M` the largest record
  the namespace admits). The plan holds at most one capacity rotation,
  the span in one file and at most one trailing gap; it parses every
  write position into a `FileOffset`, reserves the windows its round
  takes, then creates the next file, its id checked first (an id never
  wraps: `TierFlushError::FileIdsExhausted`, typed, not fatal). The parse
  and the creation are its two fallible steps, both before any writer
  effect, so a refused plan stages nothing. A round is at most 70 writes,
  4 barriers and 3 effects — 74 ops under the 8-bit op index — in capped
  holders built empty with the pipeline, with its staged seals (2); the
  created file's two directory handles are one value, held from the
  file's stage to the round's end; its windows come from fixed storage
  built whole with the pipeline, `⌈max(S, M)/Q⌉` batch windows and 5
  single ones, capacity reserved at the build and each window zeroed at
  its first take (`tier_window_capacity_bytes`,
  `tier_window_written_bytes` in INFO `# Tripwires`). The windows and the
  directory handles are fixed storage, not capped holders, and the capped
  holders are built empty, so the build fills no capped holder. Staging
  cannot fail; the round allocates nothing.
- **One round in flight per namespace** — by-construction
  (`TierNs::round: Option<FlushRound>`; a plan asserts no round is
  staged, and the pool, on its own state, that no window is out and no
  reservation held: neither is a refusal, which the pool answers only for
  a span above it).
- **No durability fact at submission**: `advance_flushed`, `durable_len`,
  seal catalog commits, and gap crossings apply only in
  `complete_flush_round`, after every op's terminal completion —
  by-construction (effects are data on the round; the only applier runs
  post-completion). The equivalence storm
  (`tiered_flush_reactor.rs`) asserts the watermark is unmoved between
  stage and completion, every round.
- **Barriers submit only after every write completed** (fdatasync covers
  only completed writes) — by-construction (`FlushRound::barriers_sent`
  gates on `pending == 0` with no write errors); conservative across
  files (a two-file round waits for both files' writes).
- **Effects apply in stage order** (`SealCommit` before the `GapCross`
  it covers — ADR-0052 D2) — by-construction (a capped holder, drained
  in order); `commit_oldest_seal` release-asserts a pending seal exists
  and hands the sealed file's handle back as its completion's value,
  `confirm_durable_to` release-asserts monotone-and-bounded.
- **Window custody**: pool windows return only in `finish_round`
  (all-terminal), and a dropped namespace parks whole in `round_drain`
  until its completions drain — by-construction; the `StableBytes`
  proof is SAFETY.md `tier_round_bytes`.
- **Barrier completion error is fatal** (fsync failure is fail-stop): re-surfaced as
  `TierFlushError::Fsync` at the next MAINTAIN → the plane's fatal arm →
  `fail_stop` — checked by `check-fsync-fail-stop.sh` (allowlisted with
  the review note); write errors resubmit byte-identical (ENOSPC latches
  admission first, ADR-0063 D4).
- **An unaddressable write position is fatal too** (ADR-0167 D4 and A1):
  the round's plan parses every write position, before the file creation
  and any writer effect; a refusal stages nothing — no op, no window, no
  file — and returns `TierFlushError::Unaddressable { path, offset_bytes }`
  from the same MAINTAIN, to the plane's fatal arm and `fail_stop`, since
  a retry would plan the same position. A staged write holds its position
  as a `FileOffset` beside its op state, so no consumer parses a
  tier-round position and no table beside the ops exists; a write-error
  resubmission emits the staged position as it is. `is_fatal` names the
  causes (with a span above the pipeline's pool), and none is
  storage-full.
- **Deliberately unchecked**: op-index/kind agreement between the token
  and the round table is a `debug_assert` (the generation check already
  rejects cross-round routing); resubmitted write bytes are not
  re-CRC'd (windows are never written after stage — custody, not
  checksum).

### A.8 — Device budget (M4.5-S36, ADR-0088 D2b; ADR-0178)

`DeviceBudget` (`inf-runtime/src/budget.rs`) and `SealPace`:

- **Foreground is never deferred** — by construction (`offer` answers
  `Now` before any credit arithmetic for `is_foreground()` classes and
  for an unbudgeted direction); INFO `io_budget_deferrals_{foreground}`
  reads 0 by construction — a non-zero value there is an internal-
  invariant bug, not a metric.
- **Three outcomes, one place** — type: a producer sees only `Issue`; the
  attainable, deferred and unattainable split and the overrun are private
  to `budget.rs` and resolved in `offer` alone. Review-only, with the
  reason: a `matches!`, `if let` or `let … else` collapse on those private
  enums compiles (the wildcard lints see `match` arms only); the
  population is `offer`, one site.
- **Holds or owes, never both** — type: per axis the credit is `Held(h ≤
  cap)` or `Owed(non-zero)`; a debtor is granted nothing.
- **Bounded everything**: every class's credit ≤ its cap and every pool ≤
  its cap (`min` on every refill and refund); a class owes at most
  `(largest offer − cap)⁺` plus its charges, because an overrun needs the
  class rested at its cap; `offer` never allocates or waits.
- **An idle producer holds no overrun back** — construction: a grant
  refunded in full leaves credit, debt, pool, rest state and `spent` as
  they were (the receipt; the rest state written by refill alone), and a
  tier round with nothing to stage makes no offer.
- **Progress** — the class oracle
  `no_background_offer_waits_past_its_bound`: every background class × seven hostile offers (1, cap − 1, cap,
  cap + 1, cap + pool, cap + pool + 1, the largest producer offer) × five
  shares × three refill intervals × seven regimes (idle, pool drained,
  foreground at 10× the share, foreground at the keep-up crossover, a
  same-class overrunner before and after, a zero-work sibling) — 2 940
  cases, the invariants checked after every call, every issue checked
  against the D4 bound computed from the formula, and each regime's
  engagement asserted. Its canaries: a producer that re-offers without
  the overrun, an overrun from any held credit, a grant that ends the rest
  pass, a keep-up floor rounded on its own before the max.
- **Accounting identity** (`io_budget_bytes_c == bytes the driver saw
  under class c`, ops likewise; the two cold-read classes together ==
  the driver's reads) — asserted every seed by the `m2-device-budget`
  oracle on the sim driver's `ObservedIo`; `refund` keeps it exact.
- **A tier round issues exactly what it offers, in groups** — type: an
  op reaches the driver only inside a `GrantedGroup`, made only by a
  `Now` answer to an offer of the group's exact device bytes and ops; the
  tier class's declared byte slice is the group's byte bound itself (one
  constant, `TIER_GROUP_BYTES_MAX`) and a const assertion keeps a group's
  ops within the ops slice, so no tier offer is above the cap. Tests: the `tier_cell` producer
  tests over a real budget (an 8 MiB round issues in groups, none above
  the cap; a retry group is offered and charged again; a plan refused
  part-way issues what it staged), the wire tests (`io_budget_bytes_tier_flush`
  equal to `tiering_flush_bytes` once every round completed without a
  retry; `io_budget_unattainable_tier_flush` 0 with a deferral), and the
  `m4-tier-budget` scenario's oracles on every leg: no offer above the
  cap and one pass's tier bytes within the cap plus the pool from the
  model, every ready group's wait within `n × T_c` from the model, the
  spend equal to the sim driver's tier bytes and ops (the ops a cut caught
  between push and submit added), one plan per cell turn judged per
  iteration — each with a planted canary.
- **Rate bound** (background bytes over a run ≤ `share × elapsed + 2 ×
  horizon + Σ slices`, plus one largest block per class that can
  overrun) — the `m2-device-budget` oracle.
- **Engagement, progress, foreground bound, liveness** — the same oracle:
  the budget deferred ≥ 1 (non-vacuous), ≥ 1 checkpoint published and
  zero-fill landed (no starvation), `write_stall_max_us ≤ (frame + one
  background block + horizon bytes) / disk rate + base·(1 + tail) + one
  scheduler step` (physics in both modes; guards a budget bug that admits
  two blocks at once). On seeds ≡ 1 mod 4 values are padded above the
  checkpoint cap plus the pool: an offer above the cap must be counted
  (else the run is vacuous) and every checkpoint block issued within
  `T_ckpt` of its first offer (`ckpt_block_wait_ns_max`); on the other
  seeds no class counts one (the control leg).
- **Determinism** — pinned by
  `two_budgets_fed_the_same_sequence_agree_exactly`; the sim's trace hash under `--verify-determinism`.
- **Seal pace never defers a drained cell** — by-construction (the LOG
  step asks only when `!staging.drained()`); pinned by
  `seal_pace_paces_a_pipelined_cell_and_never_a_drained_one`.
- **Deliberately unchecked**: the model's truth. A stale or optimistic
  probe file makes the budget a ceiling the device cannot meet; the
  tripwires are `barrier_class_degraded` and the deferral rates, both
  visible, neither automatic (ADR-0088 D6).

### A.9 — Shadow-slot tickets and the record pin (M4.5-S37, ADR-0093)

`TieredTable::shadow` (`inf-store/src/tiered/shadow.rs`) and
`AddressSpace::record_pin`:

- **A ticket is an exact-hash pair** — `(hash, cold, winner)` with
  `contains_pair(hash, cold)`, `contains_pair(hash, winner)`, `cold`
  below the head and `winner` RAM-resident — asserted at
  `register_shadow` (release asserts: a ticket over a slot that does
  not exist or a record the pin cannot keep is a violated invariant);
  the write path's candidate comes from `Index::each_exact` (the
  sidecar's 64 bits), never `lookup`'s 22-bit fingerprint.
- **The winner never goes cold while its ticket is open** — the record
  pin is the oldest unresolved winner (`sync_shadow_pin` after every map
  change) and `release_ceiling = min(flushed, walk pin, record pin)`;
  `advance_head` asserts `to ≤ release_ceiling()` (release); pinned by
  `the_record_pin_clamps_release_until_resolution` and
  `a_shadowed_winner_never_goes_cold`; `resolve_shadow` release-asserts
  the winner is not cold when it runs.
- **A checkpoint walk fixes each record's form when it begins** — an
  entry at or above the walk watermark is an image, and so is the winner
  of a ticket open as the walk began (the walk latches those winners);
  every other entry is a reference, and the 0x05 section lists the
  references' entries only, so an imaged extent record is registered
  once, when recovery appends its image (ADR-0184 D1–D5).
  `WalkLatch::form` is the one producer of a `WalkForm`; the walk's two
  index passes (`ckpt_walk_slice_bounded`) and its 0x05 pass
  (`extent_ckpt_entries_from`) read nothing else, in every writer. The
  walk pin holds release at the lowest latched winner, which the table
  hands it from the latch as the walk begins, until the walk ends, so a
  ticket that ends mid-walk leaves its winner imaged and RAM-resident.
  What that holds: the bytes from the lowest latched winner up to the
  watermark, at most the pinned suffix as the walk began (throttled at
  `MEM-BUDGET / 8` and bounded by the committed window, ADR-0093 A6),
  for one walk; a write that finds the window full meanwhile parks on
  the tail stall, as under any release pin. `tiering_shadow_pinned_bytes`
  stops counting those bytes when the last ticket ends and no gauge
  names the walk's hold: it is part of `flushed − head` on the
  namespace's `tiering_ns<id>` line.
  Pinned by
  `an_imaged_ticket_winner_is_not_also_a_blob_reference_entry`,
  `a_ticket_that_ends_before_the_images_keeps_its_winner_imaged`,
  `a_ticket_that_ends_after_the_images_keeps_its_blob_entry_out_of_the_section`
  and `a_walk_pins_release_at_its_lowest_image_until_it_ends`; canaries
  `inf_canary_ckpt_image_listed`, `inf_canary_ckpt_form_unlatched` and
  `inf_canary_ckpt_pin_at_watermark`.
- **Nothing is removed on hash evidence** — the only slot removal the
  module performs (`resolve_shadow`, same key) follows
  `decode_record(image).key == record(winner).key` on the verbatim cold
  image, after re-validating the ticket, both pairs and the winner's
  residency post-suspension; a foreign image is a `Collision` that
  removes nothing (`a_foreign_image_is_a_collision_and_removes_nothing`).
- **Exact death, bounded origin chain** — the same-key removal calls
  `note_death(cold, image.len())` (the space identity and the per-file
  counters move by the truth — `check_accounting` after every step in
  the store suite) and chains the twin plus its own origins into the
  winner's list, release-asserted `≤ RELOC_ORIGIN_CAP` (admission
  refuses a candidate whose list has no room — `fallback_origin`).
- **A winner is never deleted under an open ticket** —
  `TieredTable::delete` release-asserts no ticket names the address; the
  plane's `delete_one` resolves first (a Foreground read of the twin);
  replay's `apply_delete` ends the ticket instead (the crashed life's `DEL`
  already carried the same-key twin's marker, or told it apart as a
  collision) — pinned by `deleting_a_winner_with_an_open_ticket_panics`
  and the collision half of
  `recovery_appliers_reform_pairs_in_both_orders`.
- **A ticket's cold slot is never relocated** — `compaction_apply`
  defers it (blocks finalization), `try_promote` skips it; pinned by
  `compaction_defers_a_tickets_cold_slot_until_resolution` and
  `promotion_skips_a_tickets_cold_slot`.
- **The ticket set is a projection of the index** — re-formed by
  `apply_image`/`apply_extent_image` (RAM insert over exact cold slots)
  and `apply_ref` (cold ref beside an exact RAM slot), dropped by every
  slot removal (`shadow_note_removed`) and retargeted by every repoint
  (`shadow_note_moved`); pinned by
  `recovery_appliers_reform_pairs_in_both_orders`, the `m4-recovery` cardinality oracle (`len() ==
  model.len()` at quiescence) and the `m4-tiered` quiescence oracle
  (Σcells `DBSIZE` == model live keys; `live + dead == allocated`).
- **Release runs during boot replay (ADR-0174 D3)**, so a replayed
  winner is never sealed with its twin unverified, and cold slots at the
  rebuild can be this life's. A namespace that demoted settles every
  same-key pair before the rebuild, which then tickets only distinct keys
  that share a hash. The rebuild's settle reads its slot through replay's
  checked read: bytes whose key does not hash to the slot's hash are a
  typed boot refusal.
- **Bounded everything** — tickets ≤ `SHADOW_TICKETS_CAP`, the pinned
  suffix ≤ `MEM-BUDGET / 8`, reads in flight ≤ `SHADOW_READS_IN_FLIGHT`;
  each exhaustion is a counted refusal that leaves the synchronous
  verify in force (`admission_refuses_at_every_bound_and_counts`,
  `reads_in_flight_are_bounded_and_failures_retry`).
- **Cardinality** — `len() == index.len() − open tickets`; pinned across
  the store suite and both DST oracles.
- **Deliberately unchecked**: a genuine 64-bit collision cannot be
  constructed from real keys in a test (`lookup` asserts the hash is
  the key's own); the probe order it relies on is pinned at the index
  level with a forced pair (`ram_verified_slot_outranks_a_cold_twin`)
  and the verdict with a foreign image. The reconciler's *liveness*
  under sustained load is a campaign fact (the fallback counters), not
  an invariant.
