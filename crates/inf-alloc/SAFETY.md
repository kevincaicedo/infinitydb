# SAFETY inventory — `inf-alloc`

Unsafe-leaf crate (master plan §17.3). Every `unsafe` block in this crate is
listed here with its invariant and its test coverage. CI runs this crate's
unit tests under Miri.

| Location | Invariant | Coverage |
|----------|-----------|----------|
| (none yet — `buffer_pool` is 100% safe) | | |

Rules:
- New `unsafe` requires: an entry here, a `// SAFETY:` comment at the block
  (clippy `undocumented_unsafe_blocks` is `deny`), and a Miri-covered test.
- mmap-backed arena chunks (M0-S13): pointer provenance must stay within the
  mapped region; chunk lifetime outlives every `ArenaAddr` handed out —
  enforced by the arena owning all chunks for its own lifetime and `ArenaAddr`
  being meaningless without the owning arena.
