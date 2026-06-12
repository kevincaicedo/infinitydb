# SAFETY inventory — `inf-alloc`

Unsafe-leaf crate (master plan §17.3). Every `unsafe` block in this crate is
listed here with its invariant and its test coverage. CI runs this crate's
unit tests under Miri.

| Location | Invariant | Coverage |
|----------|-----------|----------|
| `arena.rs::map_chunk` (`libc::mmap`) | anonymous private mapping, result checked against `MAP_FAILED` before use | unit tests + Miri (`storm_reconciles_byte_exact`) |
| `arena.rs::unmap_chunk` / `Drop` (`libc::munmap`) | base/len are exactly one live mapping owned by the arena; entry zeroed after unmap so stale addrs hit the bounds assert, never the dead pointer | `huge_allocations_map_and_unmap`, `stale_huge_addr_panics_not_ub` |
| `arena.rs::bytes`/`bytes_mut` (`from_raw_parts[_mut]`) | offset+len bounds-checked against the owning chunk's mapped length before the slice is formed; `&self`/`&mut self` provide aliasing discipline; chunk memory lives until unmap/drop | whole arena test suite under Miri |

`buffer_pool` remains 100% safe code.

Rules:
- New `unsafe` requires: an entry here, a `// SAFETY:` comment at the block
  (clippy `undocumented_unsafe_blocks` is `deny`), and a Miri-covered test.
- mmap-backed arena chunks (M0-S13): pointer provenance must stay within the
  mapped region; chunk lifetime outlives every `ArenaAddr` handed out —
  enforced by the arena owning all chunks for its own lifetime and `ArenaAddr`
  being meaningless without the owning arena.
