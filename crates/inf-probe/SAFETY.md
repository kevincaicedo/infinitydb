# SAFETY inventory — `inf-probe`

`deny(unsafe_code)` crate with exactly one audited module allow (the
ADR-0049 shape): `src/evict.rs`. Everything else is safe code — the
`libc` dependency is otherwise constants only (`O_DIRECT`, `O_DSYNC`).

| Location | Invariant | Coverage |
|----------|-----------|----------|
| `evict.rs::fadvise_dontneed` (`libc::posix_fadvise`) | the descriptor is the borrowed `File`'s live fd for the call's duration; arguments are three plain integers (`offset 0, len 0` = the whole file, POSIX-defined); no pointer crosses the boundary; a nonzero return is an errno surfaced typed | `evicts_every_regular_file_under_the_directory` (Linux unit test; the call runs against real files) |

Rules:
- New `unsafe` requires: an entry here, a `// SAFETY:` comment at the block
  (clippy `undocumented_unsafe_blocks` is `deny`), and a test that runs it.
- The crate is dev-tool tier (never cell-resident); no Miri coverage is
  required for a pointer-free syscall, and none is claimed.
