# M2 interface freezes — log spine (draft until M2 exit)

Companion to `interfaces-m0.md`, same contract: these interfaces freeze at
**M2 exit**; changing a frozen one afterwards requires an ADR. Until the
milestone exits they are *drafts* — changes before exit still record their
reasoning in the owning ADR. Status column tracks arrival.

Formats defined by ADR-0011 unless noted.

| Interface | Crate | Status |
|-----------|-------|--------|
| Log record format v1 | `inf-log` | implemented (M2-S01) |
| Batch frame layout v1 | `inf-log` | implemented (M2-S01) |
| LSN addressing | `inf-log` | implemented (M2-S01) |
| Segment naming + lifecycle | `inf-log` | implemented (M2-S02) |
| `SegmentFs` injection seam | `inf-log` | implemented (M2-S02; extended by S05/S11/S16) |
| `MutationEffect` → record seam | `inf-store` → `inf-log` | implemented (M2-S03, ADR-0012; store-side emission + dep edge land at S08) |
| Log staging domain (`StagingRing`) | `inf-log` | implemented (M2-S03; reactor wiring at S05) |
| Sequential read path (`SegmentReader`) | `inf-log` | implemented (M2-S04; `BackendDriver` reads at S05; S14 adds tail policy) |
| Durability watermark contract | `inf-log`/`inf-runtime` | implemented (M2-S05/S06, ADR-0013; store-side wiring at S08, sim disk at S18) |
| Driver file ops (`LogWrite`/`Fdatasync`) | `inf-runtime` | implemented (M2-S05, ADR-0013 D1 — extends the frozen M0 `BackendDriver` contract) |
| `.ick` checkpoint format v1 | `inf-log` | pending (M2-S10) |
| MANIFEST schema v1 | `inf-log` | pending (M2-S11) |
| Fault-point registry | `inf-foundation`/`inf-log` | pending (M2-S16) |
| Persistence counter set | `inf-log`/`inf-bench` | pending (M2-S21) |

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
  `4 NsOp {opaque payload}` (vocabulary owned by M2-S08).
  Tags 5+ reserved: `ckpt-begin` (S10), M3 collection ops.
- `flags`: reserved, v1 defines no bits. Unknown flags and unknown types
  are **fail-stop** decode errors — replay refuses, never skips (§8.4).
- Public surface: `RecordView<'_>` (borrowing views; invalid records are
  unrepresentable), `RecordView::encode_into(&mut Vec<u8>)` /
  `encoded_len()`, `decode_record(&[u8]) -> (RecordView, consumed)`,
  `NsId(u32)`, `RecordDecodeError`.

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
  `frame_len()`, `finalize(first_record_lsn) -> &[u8]`, `sealed_frame()`
  (post-finalize re-access for the leased in-flight write), `reset()`),
  `decode_frame(&[u8], max_frame_len)`, `FrameRef::records()` yielding
  `(Lsn, RecordView)`, `FrameIter` (validate-then-yield; stops at zero
  magic; `offset()` = bytes consumed — the tail-scan input for S04/S14),
  `FrameDecodeError`, `FrameRecordError`.

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
  non-recoverable `FsyncFailed` type (§8.4 fsyncgate rule — CI greps for
  it in non-fatal match arms from S17).
- Append protocol: `begin_frame(len, now) -> FrameSlot` (rotates if
  needed, reserves the base LSN) → `FrameBuilder::finalize(slot.
  first_record_lsn())` → `commit_frame(slot, bytes) -> Lsn`.

## `SegmentFs` injection seam (`inf-log::fs`)

Control-path file operations (create/prealloc, dir-fsync, list, open,
positional read/write, fdatasync) behind a trait so DST can fault every
one (L7). Tiers: `StdSegmentFs` (boot/dev; `set_len` prealloc — real
`fallocate` arrives with the S05 BackendDriver file ops), `fs::mem::MemFs`
(deterministic, fault-injectable test tier), the M2-S18 sim disk. The
per-iteration hot-path write + linked fsync is **not** part of this seam —
it rides `BackendDriver` (S05).

## `MutationEffect` → record seam (`inf-log::effect`, ADR-0012)

```rust
enum MutationEffect<'a> {
    StringSet { ns: NsId, key: &'a [u8], value: &'a [u8] },  // → tag 1 post-image
    Delete    { ns: NsId, key: &'a [u8] },                   // → tag 2
    ExpireAt  { ns: NsId, at_unix_ms: u64, key: &'a [u8] },  // → tag 3 (absolute)
    NsOp      { ns: NsId, payload: &'a [u8] },               // → tag 4
}
```

- `record() -> RecordView<'a>` is the encoder registry: M3 collection ops
  and M6 doc deltas add variants + record tags here without touching the
  frame spine. `encoded_len()` is exact (admission + accounting input).
- Defined in `inf-log` (the seam's consumer — §3.3); `inf-store` imports
  it when S08 wires durable namespaces (the dep-DAG edge lands there,
  direction fixed by ADR-0012).

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
- `SegmentFile::raw_fd() -> Option<RawFd>` (`None` on in-memory tiers; the
  reactor path requires a real-file tier — the sim implements the driver
  ops themselves, S18).
