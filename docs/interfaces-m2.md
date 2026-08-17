# M2 interface freezes — log spine (draft until M2 exit)

Companion to `interfaces-m0.md`, same contract: these interfaces freeze at
**M2 exit**; changing a frozen one afterwards requires an ADR. Until the
milestone exits they are *drafts* — changes before exit still record their
reasoning in the owning ADR. Status column tracks arrival.

Formats defined by ADR-0011 unless noted.

| Interface | Crate | Status |
|-----------|-------|--------|
| Log record format v1 | `inf-log` | implemented (M2-S01) |
| Batch frame layout v1 | `inf-log` | implemented (M2-S01); **read-only since M2.5-S12** (ADR-0031 — v2 is the written format; v1 accepted forever on the alpha line) |
| Batch frame layout v2 (per-frame sequencing) | `inf-log` | implemented (M2.5-S12, ADR-0031 — epoch · seq · covered-LSN stamp under the CRC; the tail-scan attestation taxonomy consumes it) |
| LSN addressing | `inf-log` | implemented (M2-S01) |
| Segment naming + lifecycle | `inf-log` | implemented (M2-S02) |
| `SegmentFs` injection seam | `inf-log` | implemented (M2-S02; extended by S05/S11/S16; sim-disk tier at S18) |
| `MutationEffect` → record seam | `inf-store` → `inf-log` | implemented (M2-S03/S08, ADR-0012/0015; dep edge + command-layer post-image emission live) |
| Document record classes + replay contract | `inf-doc`/`inf-store`/`inf-log` | implemented (M3-S17, ADR-0043 — tags 6/7, incarnation lineage, recorded replay witnesses, static opcode registry, modular-u24 replay, checkpoint full images) |
| Log staging domain (`StagingRing`) | `inf-log` | implemented (M2-S03; reactor wiring at S05) |
| Sequential read path (`SegmentReader`) | `inf-log` | implemented (M2-S04; `BackendDriver` reads at S05; S14 tail policy implemented — ADR-0018) |
| Durability watermark contract | `inf-log`/`inf-runtime` | implemented (M2-S05/S06/S08, ADR-0013/0015; live on `ServerPlane` — acks seq-keyed, semantics unchanged; sim disk at S18) |
| Driver file ops (`LogWrite`/`Fdatasync`) | `inf-runtime` | implemented (M2-S05, ADR-0013 D1 — extends the frozen M0 `BackendDriver` contract) |
| Node catalog `META` swap (`inf-log::meta`) | `inf-log` | implemented (M2-S08, ADR-0015 D3 — the S11 MANIFEST protocol class; payload = `inf-store::catalog` v1) |
| Namespace selection + `Op::ApplyNs` | `inf-server`/`inf-fabric` | implemented (M2-S08, ADR-0015 D1 — the ADR-0009 §4 codec revision, additive opcode 6; `ns ≥ 16` enforced at decode) |
| `.ick` checkpoint format v1 | `inf-log` | implemented (M2-S10, ADR-0016 — record-v1 payload; digest = hash64 chain, recorded deviation from "xxh3") |
| Checkpoint scheduler group + ckpt token classes | `inf-runtime` | implemented (M2-S10/S11, ADR-0016 D4/D5 + ADR-0017 D3 — `GroupClass::Checkpoint`; `TokenClass::{CkptWrite,CkptSync,ManifestSync}` routing-only extensions of the frozen M0 token contract) |
| MANIFEST schema v1 | `inf-log` | implemented (M2-S11, ADR-0017 — `INFMAN1\0` payload in the META envelope class; swap steps ride the driver via `TokenClass::ManifestSync` on the reactor tier) |
| Tail-region scan + corruption taxonomy | `inf-log::tail` | implemented (M2-S14, ADR-0018 — `scan_region`/`RegionScan` facts + `LogCorruption`; policy in `inf-server::recover`) |
| Recovery state digest | `inf-store` | implemented (M2-S13, ADR-0018 — `Keyspace::state_digest`, order-independent multiset hash; the S18/S19 oracle currency) |
| Fault-point registry | `inf-foundation`/`inf-log`/`inf-server` | implemented (M2-S16/S17, ADR-0019/0020 — thread-local, feature-gated `fault-points`, deterministic triggers; 8 points wired + CI inventory check across all decl modules) |
| Crash matrix (definition-as-data + runner) | `tests/crash-matrix` | implemented (M2-S17, ADR-0020 — `m2.toml` rows × policies × workloads × seeds, kill-and-recover on `MemFs`; fsyncgate child-process row; per-PR via workspace tests + expanded-seed nightly) |
| Sim disk (lose/tear/reorder) | `inf-log::fs::sim` | implemented (M2-S18, ADR-0020 — `SimDisk`: volatile-until-fsync data, dir-fsync-ordered metadata, seeded sector-granular power cuts; driver ops executed by the `inf-sim` `SimDriver`) |
| Durable plane over `SegmentFs` (generic) | `inf-server` | implemented (M2-S19, ADR-0021 D1 — `ServerPlane<O, F = StdSegmentFs>`, `DurableCell/CkptCell/ManifestCell/BootRecovery<F>`; `begin_recovery(fs, …)`; monomorphized, zero std-tier change) |
| Detached control plane (`ControlInbox`) | `inf-server` | implemented (M2-S19, ADR-0021 D2 — `ControlHandle::detached[_with_catalog]` + inline `drain(fs, dir)`: catalog swaps + unlinks inside the deterministic sim loop; `load_catalog_from<F>`) |
| Durability oracle + sweep (`m2-durable`) | `inf-sim` | implemented (M2-S19, ADR-0021 D3-D5 — ack-stream oracle incl. the survival audit on taxonomy-refused boots; `--sweep/--shard/--out`; 10k-seed gate artifact green; `Plant::FsyncLies` canary) |
| Checkpoint board (`CkptBoard`/`CkptSlot`) | `inf-server::control` | implemented (M2-S20, ADR-0021 D6 — per-cell request/publication epoch slots; publication at the MANIFEST swap's dir-fsync commit; aborted swaps retry after backoff so `WAIT` cannot hang) |
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
  are **fail-stop** decode errors — replay refuses, never skips (§8.4).
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

## `first_lsn` bound — post-M2-exit amendment (ADR-0072, 2026-08-17)

Post-exit change to a frozen surface, ADR-gated per the freeze discipline.
The nightly `frame_decode` campaign found that `first_lsn.offset` was read
from the header and never bounded: a CRC-valid frame declaring
`offset = u32::MAX` made `RecordIter` advance the record cursor past the
`u32` ceiling and panic in `Lsn::advance`.

- **The derivation is per-version.** `first record = frame base +
  header_len` — 20 for v1, 40 for v2. ADR-0011 D2's text says "frame base
  + 20", which is v1-era wording from before ADR-0031's 40-byte v2 header;
  ADR-0072 D1 restates it. **Cite `header_len`, never the constant 20.**
- **`decode_frame` bounds the field** where it is read, for both formats:
  `header_len ≤ first_lsn.offset` and `(first_lsn.offset − header_len) +
  frame_len ≤ u32::MAX`. A frame's first record is never inside its own
  header, and a frame always fits inside a u32-addressed segment.
- **New public variant `FrameDecodeError::BadFirstLsn { offset: u32 }`** —
  corruption class, not the torn-tail class (`ZeroMagic` stays the only
  expected end-of-log signal). `FrameDecodeError` deliberately stays
  exhaustive (no `#[non_exhaustive]`, ADR-0072 D3): the added variant is
  semver-breaking for external matchers, accepted pre-1.0 and revisited at
  1.0. Decoder error enums grow by ADR, each naming its semver impact.
- The bound also makes `inf-server::recover`'s `first_lsn.offset −
  header_len` frame-base subtraction sound on its own, instead of relying
  on `SegmentReader::next_frame`'s physical-offset cross-check having run
  first.

## LSN addressing (`inf-log::lsn`)

`Lsn = { segment: SegmentId(u32), offset: u32 }`, per cell; ordering is
(segment, offset) lexicographic = append order. A record's LSN is the byte
offset of its length prefix within its segment. **No global LSN exists**
(master plan §8.1).

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
  or optional `seal_after_ms` (default off — M2 cut line).
- ENOSPC discipline: prealloc failure raises `space_exhausted()` *before*
  writes need the space (the S08 admission hook); appends that outrun it
  get typed `LogError::NoSpace`. fsync failure is the distinct,
  non-recoverable `FsyncFailed` type (§8.4 fsyncgate rule — enforced
  since S17 by `scripts/check-fsync-fail-stop.sh`: the fsync-error types
  may appear only in the audited allowlist of fail-stop sites; on the
  node, `DurableCell::fail_stop` exits with
  `inf_server::EXIT_DURABLE_FAILSTOP` (= 3) after freezing the watermark
  — zero acks for the affected batch, proven by the child-process
  fsyncgate test).
- Append protocol: `begin_frame(len, now) -> FrameSlot` (rotates if
  needed, reserves the base LSN) → `FrameBuilder::finalize(slot.
  first_record_lsn())` → `commit_frame(slot, bytes) -> Lsn`.

## `SegmentFs` injection seam (`inf-log::fs`)

Control-path file operations (create/prealloc, dir-fsync, list, open,
positional read/write, fdatasync) behind a trait so DST can fault every
one (L7). Tiers: `StdSegmentFs` (boot/dev; `set_len` prealloc — real
`fallocate` arrives with the S05 BackendDriver file ops), `fs::mem::MemFs`
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
- Defined in `inf-log` (the seam's consumer — §3.3); `inf-store` imports
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
  and fail-stops if the result count or canonical output length disagrees.
  This is ADR-0043 D6 / M3 §3.4 R1–R3, including recreation and version
  wrap.

## Log staging domain (`inf-log::staging`, M2-S03)

The §7.1 "log staging ring": a double-buffered frame pair (contiguity for
one-writev — ADR-0012), fixed capacity, allocated once per cell.

- EXECUTE: `stage(&MutationEffect) -> Result<StagedAt, StagingFull>` —
  in-place encode, zero steady-state allocation (counting-allocator
  enforced); `StagingFull{needed, available}` is the typed backpressure
  that stops durable read-rearm (wired S05/S08); `would_fit`/`backlogged`
  are the admission predicates.
- LOG: `can_seal()` → `seal(first_record_lsn) -> FrameLease` (staging
  swaps to the free buffer; at most one frame in flight) →
  `leased_frame(&lease)` for the writev → `release(lease)` on completion.
  `flush_into(&mut SegmentRotor, now_ms)` is the synchronous pre-S05
  choreography.
- LSN handoff (L6): `FrameLease::lsn_of(StagedAt) -> Lsn`,
  generation-checked — the S06 `WatermarkGate` registration input.
- Accounting (L5): `staged_bytes`/`in_flight_bytes` exact at every
  append/seal/release; `resident_bytes` = 2 × capacity, constant;
  cumulative `StagingStats{appends, append_bytes, refusals, seals,
  releases}`.

## Sequential read path (`inf-log::reader`, M2-S04)

`SegmentReader` over one segment (sealed or active tail), reads through
the `SegmentFs` seam (→ `BackendDriver` at S05):

- `next_frame() -> Result<Option<FrameRef>, ReadError>` (lending,
  validate-then-yield, one CRC pass per frame via header peek) and
  `apply_frames(callback) -> Result<ReadEnd, ApplyError>` (the replay
  batch-apply shape).
- Stored `first_lsn` cross-checked against physical offset per frame —
  `ReadError::LsnMismatch` on misdirected writes (ADR-0011 D2).
- Tail = facts, not policy (S14 owns the taxonomy):
  `ReadEnd::{ZeroTail, FileEnd}` with `.at()` = byte after the last valid
  frame (the `SegmentRotor::open_existing` tail_offset); torn/corrupt
  bytes are typed `ReadError::Frame{segment, offset, error}` — the reader
  never truncates and never skips.
- `ReaderConfig{chunk_bytes = 1 MiB, max_frame_len}`; window grows only
  when a frame exceeds it (bounded by `max_frame_len`).

## Driver file ops (`inf-runtime`, M2-S05, ADR-0013 D1)

Extension of the frozen M0 `BackendDriver` contract (recorded per the §3.2
freeze discipline). The token *layout* is unchanged; `TokenClass` gains
`LogWrite = 5` and `Fsync = 6`.

- `IoOp::LogWrite { fd, offset, data: StableBytes, token, fsync_token }`
  — positional write of one sealed frame (the contiguous frame makes "one
  writev" a single-iovec write). Short writes resubmit internally:
  `CompletionResult::LogWritten` ⇒ ALL bytes reached the fd (page cache —
  the staging-lease release point, never an ack point).
- `fsync_token: Some(_)` chains an fdatasync — `IOSQE_IO_LINK` on uring
  (kept unsplittable across submit boundaries), issued after the write's
  completion on fallback tiers. `CompletionResult::Synced` is the ONLY
  durability fact (L2) and is delivered exactly once, only after every byte
  of the write is both written and covered (a sync that raced a short write
  is superseded internally); a failed write cancels it
  (`Error{ECANCELED}` on `fsync_token` — no sync-past-failed-write).
- `IoOp::Fdatasync { fd, token }` — standalone barrier (everysec tick,
  deferred seal).
- `StableBytes::new(&[u8])` is the one `unsafe` seam: bytes must stay
  live/stable/unmodified until the op's terminal completion — the staging
  `FrameLease` is the canonical proof (buffers never reallocate; reset only
  on release). Construction lives in the plane; `inf-log` stays
  `#![forbid(unsafe_code)]`. S08 decides `inf-server`'s shape (ADR-0013).
- `fallocate`/rename/dir-fsync ops were deliberately NOT added (no unused
  surface); they land with S11/ENOSPC-hardening consumers.

## M2.5-S01/S07 amendments (boot barriers · sync pipeline — ADR-0026)

Post-M2-exit changes to frozen surfaces, ADR-gated per the freeze
discipline:

- **`SegmentFile::advise_read_ahead(offset, len)`** (default method,
  no-op — M2.5-S08, ADR-0028): a hint that the caller will read
  `[offset, offset + len)` sequentially next. `SegmentReader::refill`
  hints the next two windows; `IckReader::next_step` hints per section.
  Hint-only by contract: implementations must not change any byte the
  reader sees (L7 — MemFs/sim keep the no-op). The real implementation is
  `inf-server::ReadAheadFs` (boot-scoped prefetch thread; prefetch enabled
  by the assembly iff a single cell recovers — the measured regime split).
  Also under this ADR: `read_ick_counts` gained a direct end-of-file
  footer probe (CRC-validated, hops as fallback) — a parse-path change
  covered by the extended `ick_decode` fuzz oracle.
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
  every durable ack (and manifest publication) behind them — boot-ready
  never blocks on the device. A blocking metadata fsync on a reactor
  thread is the ADR-0022 D7 wedge mechanism; the boot-storm DST scenario
  (`inf-sim --scenario boot-storm`) enforces a zero blocking-sync ready
  path, and `inf-bench boot-storm` is the device-tier spawn-storm
  regression.
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
  traffic was pending).
- **Formation observables**: `CommitStats` gains `fsyncs_completion`,
  `fsyncs_boot_barrier`, `fsyncs_prealloc_barrier`; `DurableStats`/`INFO
  persistence` gain `fsync_group_p50`/`fsync_group_p99` (records newly
  covered per durability-fsync completion — the M2.5 formation gate
  observable); `gate-run m2` emits `tripwire:group_formation_x` and
  `tripwire:spawn_retries` (must read zero post-S01).

## Group commit + durability watermark (`inf-log::commit`, M2-S05/S06)

`GroupCommit<File>` — cell-local policy engine + fsync ledger; never names
ops or sockets. The plane translates (choreography table in ADR-0013 D2 and
the module docs; `inf-server` adopts it at S08):

- Inputs: `note_staged(FsyncClass{Everysec,Always})` (EXECUTE),
  `note_everysec_tick()` (plane-armed injected timer — idle ticks free),
  `register_seal_fsync(SealHandoff)` (deferred rotation, ADR-0013 D4).
- LOG: `frame_fsync_due()` → `note_frame_queued(end, len)` →
  `register_linked_fsync()` (one sync covers every class due that
  iteration — §8.2 group commit); `standalone_fsync_due()` →
  `register_standalone_fsync()` (dirty bytes, no frame; covers
  written-at-submission, never queued).
- REAP: `note_frame_written()` (lease release point);
  `on_fsync_complete(ticket) -> Option<Lsn>` — submission-ordered ledger,
  **done-prefix** advance (completions may cross fds out of order);
  `on_fsync_error(ticket)` freezes the watermark forever (§8.4 — caller
  fail-stops; observable in tests).
- Watermark = **exclusive end** of the fsync-covered range; gate key =
  `Lsn::to_u64()` (`(segment << 32) | offset`, order-preserving). `always`
  futures register at their record LSN via `FrameLease::lsn_of` and wake
  FIFO by LSN under the EXECUTE budget (wake storms drain across slices).
- Counters (S21 vocabulary): `CommitStats`, `fsync_latency_hist` (µs),
  `pending_log_bytes`, `last_durable_lsn` (= watermark), `queued_up_to`.

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
  deviation from the plan's "xxh3" (ADR-0016 D6); the version field is the
  upgrade path.
- Footer per-ns entry counts are S13's table-presizing input — realized by
  `read_ick_counts` (M2-S13, ADR-0018 D6): a header-hop footer peek (counts
  under the footer's own CRC; the streaming pass still runs the full audit)
  feeding `Keyspace::reserve_ns` before the bulk apply (measured: the
  doubling-rehash storm cost ~15% of replay throughput). Both the stream
  writer and footer-audit count `StringPostImage | DocFull` as namespace
  entries; metadata records do not inflate the presize count.
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
  writers: `IckStream::new_v2` + `stage_addr_ref`, `SyncIckWriter::
  create_v2` + `append_ref` (sections homogeneous by class, sealed at
  class/ns/watermark boundaries). Record tag **8 = `ColdDisplace {ns,
  old_addr: u48}`**: stages immediately before a tiered-namespace
  mutation that displaced a record; replay removes exactly the slot
  `(hash, old_addr)` then applies the mutation (zero disk reads — the
  hash-repoint and deferred-reconcile alternatives are rejected in the
  ADR); the walker never emits it; `Keyspace::apply_record` counts it
  `SkippedReserved` (tiered replay routes through `TieredTable::apply_*`
  until command wiring). Fuzz: `ick_decode` extended over the v2 arm.
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
  (milestone risk-table rule; deliberately narrower than §8.4 — nothing
  was acked against the checkpoint and the log stays authoritative).
- Trigger v1 (ADR-0016 D7): cell-local `interval_bytes` threshold +
  `ControlHandle::request_ckpt_all()` epoch (polled in MAINTAIN — the
  persisted-epoch pattern). One checkpoint in flight per cell; triggers
  latch, never stack. `INF.CKPT`/`BGSAVE` ride this at S20.

## Boot recovery orchestration (`inf-server::recover`/`plane`, M2-S15 — ADR-0019)

- Recovery is a **resumable state machine**: `Recovery<F: SegmentFs>` with
  phases Start → Ick → Replay → Audit → Finish, each `step(ks, budget)`
  bounded by input bytes (one frame/section overshoot). `open_cell_log`
  is the machine run to completion — one code path, so the S13
  determinism sweep and S14 taxonomy suite prove the stepped machine.
  `IckReader` is the pull-based `.ick` loader (`read_ick` reimplemented
  on it; same audit, same fuzz target).
- The node boots cells directly into their reactor loops: `ServerPlane::
  begin_recovery` (requires `set_control`) drives steps from MAINTAIN
  (`RecoverConfig{step_bytes: 8 MiB, throttle_bytes_per_sec: None}` —
  the throttle is test-only, metered on **consumed** bytes, never the
  prealloc-slack credits progress also carries). Unthrottled boots never
  park (`before_park`); recovery failure surfaces via `take_boot_error`
  → the assembly fail-stops the process (§8.4).
- **`-LOADING` gate (wire layer):** `CmdFlags::LOADING` in the command
  registry — membership pinned to *observed* Redis 8.0.5 behavior
  (capture artifact `.artifacts/m2/loading-redis-capture-20260703/`;
  notably **PING is gated** there). While `RecoveryBoard::all_ready()`
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
  machinery strings (artifact
  `.artifacts/m2/fault-points-ab-dev-20260703/`). Feature-on unarmed:
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
  (`tests/crash-matrix/m2.toml`) additionally requires a matrix row per
  point (runner-enforced). The M2-S18 sim disk consumes the same
  registry for power-cut scheduling (arm a point → observe the typed
  error → `cut_after_ops`/`power_cut`); reactor-tier write/fsync
  failures are injected by the ScriptedDriver, the sim disk's dead
  switch, and `durable_fsync_eio`.

## MANIFEST schema v1 (`inf-log::manifest`, M2-S11 — ADR-0017)

- `shard-k/MANIFEST` names the recovery unit atomically:
  `{format epoch, ckpt id, begin-LSN, live segment set}` inside the
  `inf-log::meta` envelope (magic `INFMETA1` + length + CRC32C — one swap
  protocol shared with the catalog). Payload: magic `INFMAN1\0`,
  `epoch: u32 = 1`, `ckpt_id: u64`, packed `begin: u64`, count + u32
  segment ids (strictly ascending; `segments[0] == begin.segment` = the
  truncation floor). Canonical decode: trailing bytes / empty or
  non-ascending sets / floor mismatch are named errors. v1 writes the
  contiguous `floor..=active`; decode admits holes (M5/M7 publish them).
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
  Manifest present but named state missing/corrupt = fail-stop, never a
  full-replay fallback. `open_cell_log` is generic over `SegmentFs` (DST
  seam); recovery is digest-deterministic (`Keyspace::state_digest`,
  same-files ⇒ same digest + same resume LSN, CI-asserted).
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
- **M4-S15 amendment (ADR-0059) — retirement + unlink lifecycle.** The
  §3.1 deletion conjunction made mechanical, staged around the MANIFEST
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
     is the **only** constructor of `SealedExtent` — reference position
     (`TieredTable::insert_extent`/`update_extent`,
     `MutationEffect::StringSetExtent`) requires the token, so "extent
     durable before referencing ack" is structural on the sync tier
     (the reactor tier's coverage-neutral `GroupCommit` ledger barrier
     is command wiring's named obligation). Extent **fsync failure is a
     typed abort** (extent abandoned, id quarantined, never retried —
     the one ADR-audited narrower §8.4 posture; the module is
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
| 1.3 | `register_linked_fsync`: `!always_unqueued` — every owed `always` record already rode a queued frame before the linked sync discharges the due | A linked sync clearing `always_pending` while an `always` record is still unqueued → its ack gates on a sync that does not cover it (S06 oracle violation) | `debug_assert!` commit.rs:432 | **promote** — per-batch (only on `always` frames), free |
| 1.4 | `register_seal_fsync`: `queued_bytes == written_bytes` — rotation only with no write in flight (seal covers a complete segment) | A seal fsync registered mid-write claims coverage of bytes not yet on the old segment | `assert_eq!` **release** commit.rs:340 | keep |
| 1.5 | `register_boot_barrier`: `frames_queued == 0` **and** `durable_up_to.is_none()` — barriers register at the ledger head before any durable traffic | A barrier behind real data would (a) not fence acks and (b) risk covering data with a zero-byte floor | two `assert!`/`assert_eq!` **release** commit.rs:372, 373 | keep (per-boot) |
| 1.6 | `on_fsync_complete`: ticket exists and `!done && !failed` — completions are exactly-once (driver contract) | A double-completion or unknown ticket corrupts the done-prefix bookkeeping | `expect` + `assert!` **release** commit.rs:538, 539 | keep |
| 1.7 | `on_fsync_error`: ticket exists and `!done` | An error on an already-done entry means the ledger and driver disagree | `expect` + `assert!` **release** commit.rs:570, 571 | keep |
| 1.8 | done-prefix advance: watermark only moves through contiguous `done && !failed` front entries | A failed/pending entry must freeze everything behind it (fsyncgate) | by-construction (loop breaks on `!done \|\| failed`) commit.rs:549–556 | keep |
| 1.9 | `register_completion_fsync`: `always_discharged_at_written()` before issuing | A completion-issued sync that does not cover an owed `always` record acks it early | `debug_assert!` commit.rs:477 (guarded by `completion_fsync_due` at call site) | keep — call-site predicate is the real guard; debug_assert is the pair |
| 1.10 | `with_sync_pipeline`: `bound ∈ 1..=2` — the pipeline is bounded, never a queue (L3) | An unbounded in-flight sync count is the batch=1.0 disease reborn | `assert!` **release** commit.rs:230 | keep (per-construction) |
| 1.11 | `note_everysec_tick`: idle tick (clean + no `always`) issues no sync | A sync on a clean tick is wasted device work, not a correctness bug | by-construction (branch) commit.rs:267 | keep |
| 1.12 | `register_prealloc_barrier`: coverage-neutral — enters at the current coverage tail, never advances past real data | A dir sync that claimed data coverage would advance the watermark past unfsynced frames | by-construction (copies tail `covers_*`) commit.rs:386–394; pinned by `prealloc_barrier_is_coverage_neutral` test | keep |

**Verdict:** exceptionally well-asserted machine (32 `assert!` / 34
`assert_eq!` / 4 `debug_assert!`). The three durable-honesty invariants that
are still debug-only (1.1–1.3) are the promotion headline — all per-batch,
so promotion is free.

---

## 2. `WatermarkGate` — LSN-keyed ack gate (`inf-runtime/src/gate.rs`)

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

Double-buffered frame pair; the **generation token** is the custody
mechanism (a stale `StagedAt`/`FrameLease` resolving to the wrong LSN would
mis-address a record → wrong data acked as durable).

| # | Invariant | Why it must hold | Current enforcement | Disposition |
|---|---|---|---|---|
| 3.1 | `lsn_of`: `at.generation == self.generation` — token belongs to this lease | A cross-generation token resolves a record to a bogus LSN → an ack gates on the wrong watermark | `assert_eq!` **release** staging.rs:138 | keep (the custody invariant; already release — exemplary) |
| 3.2 | `leased_frame`: `in_flight.generation == lease.generation` | Writing the wrong buffer's bytes to the segment | `assert_eq!` **release** staging.rs:349 | keep |
| 3.3 | `release`: `in_flight.generation == lease.generation` | Releasing the wrong buffer resurrects in-flight bytes | `assert_eq!` **release** staging.rs:357 | keep |
| 3.4 | `seal`: `!is_empty()` and `in_flight.is_none()` — at most one frame in flight, no empty seal | A second seal while a lease is outstanding aliases the in-flight buffer (memory > 2×capacity, torn write) | two `assert!` **release** staging.rs:328, 329 | keep |
| 3.5 | `new`: `capacity_bytes ∈ [min_frame, DEFAULT_MAX_FRAME_LEN]` | A capacity below a minimal frame or above the reader bound writes frames no default reader can replay | two `assert!` **release** staging.rs:204, 206 | keep (per-boot) |
| 3.6 | `stage`: record over remaining capacity → typed `StagingFull`, effect not partially staged | Backpressure is an operating condition, not an invariant (L: bounded queue) | checked-error staging.rs:227–230 | keep |
| 3.7 | oversized record (`> max_record_len`) is admission's problem, never retried here | Retrying an un-stageable record is a livelock | by-construction + doc (admission checks `max_record_len` up front) staging.rs:254–260 | keep — verify S08 admission actually calls `max_record_len` (out of scope here; note for cross-check) |

**Verdict:** the custody invariants are already release asserts — the
model chapter for this whole audit. Nothing to promote.

---

## 4. Checkpoint + MANIFEST swap (`inf-server/src/ckpt.rs`)

`CkptPhase` (stream) and `SwapPhase` (recovery-unit transition). Failure
policy here is **abort, never fail-stop** (nothing is acked against an
in-flight checkpoint) — deliberately narrower than the log path's §8.4.
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

**Verdict:** the two publication invariants (4.1, 4.2) that stand between a
crash and a corrupt recovery unit are debug-only. Promote both — free.

---

## 5. `Recovery` state steps (`inf-server/src/recover.rs`)

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
