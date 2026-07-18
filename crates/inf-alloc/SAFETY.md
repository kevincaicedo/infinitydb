# SAFETY inventory — `inf-alloc`

Unsafe-leaf crate (master plan §17.3). Every `unsafe` block in this crate is
listed here with its invariant and its test coverage. CI runs this crate's
unit tests under Miri.

| Location | Invariant | Coverage |
|----------|-----------|----------|
| `arena.rs::map_chunk` (`libc::mmap`) | anonymous private mapping, result checked against `MAP_FAILED` before use | unit tests + Miri (`storm_reconciles_byte_exact`) |
| `arena.rs::unmap_chunk` / `Drop` (`libc::munmap`) | base/len are exactly one live mapping owned by the arena; entry zeroed after unmap so stale addrs hit the bounds assert, never the dead pointer | `huge_allocations_map_and_unmap`, `stale_huge_addr_panics_not_ub` |
| `arena.rs::bytes`/`bytes_mut` (`from_raw_parts[_mut]`) | offset+len bounds-checked against the owning chunk's mapped length before the slice is formed; `&self`/`&mut self` provide aliasing discipline; chunk memory lives until unmap/drop | whole arena test suite under Miri |
| `counting_allocator.rs::GlobalAlloc` (test/feature-only) | every operation delegates the caller's pointer/layout contract unchanged to `System`; the relaxed counter does not alter allocation semantics | `delegates_and_counts_allocations` + `inf-doc/tests/scalar_patch_alloc.rs`; Miri compiles the unit-test arm |
| `region.rs::map_reservation` (`libc::mmap`, both cfg arms) | anonymous private mapping (PROT_NONE + NORESERVE; READ\|WRITE under Miri), no fixed address, result checked against `MAP_FAILED` before use | region unit tests under Miri (`commit_write_read_round_trip`, spans, recommit) |
| `region.rs::protect_read_write` / `release_and_protect_none` (`mprotect`/`madvise`) | page-aligned range inside the live reservation, asserted against the per-page commit bitmap by the callers; DONTNEED/FREE only on committed private anonymous pages; return codes asserted (a failed protect is a violated invariant, not an operating error) | Linux unit tests (`decommit_then_recommit_reuses_pages`); elided under Miri (unsupported shims — the bitmap asserts still run) |
| `region.rs::bytes`/`bytes_mut` (`from_raw_parts[_mut]`) | offset+len bounds-checked against the reservation; touched pages committed (debug-asserted per page; release builds rely on the owner's watermark discipline, M4-S01); `&self`/`&mut self` provide aliasing discipline; mapping outlives the region | whole region test suite under Miri |
| `region.rs::Drop` (`libc::munmap`) | base/len are exactly the one live mapping created in `map_reservation`, owned for the region's lifetime | every region test's drop |

`buffer_pool` remains 100% safe code.

Rules:
- New `unsafe` requires: an entry here, a `// SAFETY:` comment at the block
  (clippy `undocumented_unsafe_blocks` is `deny`), and a Miri-covered test.
- mmap-backed arena chunks (M0-S13): pointer provenance must stay within the
  mapped region; chunk lifetime outlives every `ArenaAddr` handed out —
  enforced by the arena owning all chunks for its own lifetime and `ArenaAddr`
  being meaningless without the owning arena.
