# inf-doc SAFETY

`inf-doc` carries exactly **one** audited unsafe region: the
[`emit`](src/emit.rs) module (ADR-0049 — the ADR-0047 D3 escalation that
amended the L9 leaf list). Every other module is `deny(unsafe_code)`
(crate-level), and the crate exposes no unsafe API.

The region's single contract, every block: **reserve _n_, write at most
_n_ bytes through a raw cursor, `set_len` to exactly what was written.**
No intrinsics, no `transmute`, no pointer outlives its call, no
uninitialized byte is ever exposed.

## `emit.rs` — reserve-once/write-unchecked primitives (ADR-0049)

- **`i64` / `i64_into_raw`**: `reserve(I64_MAX_LEN = 11)` before the raw
  write; the fixint arm writes 1 byte, the varint arm at most 1 + 10 (a
  zigzagged u64 spans ≤ 10 septets); `set_len` adds the exact count the
  writer returned. `i64_into` reuses the same writer against a caller
  `[u8; 11]` — the array type carries the bound.
- **`f64`**: `reserve(F64_LEN = 9)`; one tag byte + one 8-byte array
  store; `set_len(+9)`.
- **`str_header`**: `reserve(4)`; the three width arms write 1, 2, and 4
  bytes respectively and `set_len` exactly that. The str24 arm's single
  4-byte store packs tag + u24 (`len < 2²⁴` is the format ceiling,
  enforced upstream by `DOC_BYTES_MAX` ≤ 16 MiB − 1).
- **`begin`**: `reserve(4)`; one 4-byte store (tag + zeroed u24
  placeholder); `set_len(+4)`.
- **`append_overlapped`**: `reserve(len + 7)` bounds the overshooting
  word stores (each begins at `< len`, writes 8); the happy path
  `set_len(+len)` exposes only source-copied bytes; the slack-exhausted
  path first `set_len`s the words actually written, then grows through
  safe `extend_from_slice`.

**Verification**: unit tests in-module compare every primitive
byte-for-byte against safe oracles and run under Miri
(`cargo +nightly miri test -p inf-doc emit`); the golden suite pins the
full format from both drivers; the 10⁶-document differential and the
`json_parse`/`idoc_decode` fuzz targets drive the region continuously
(L9 fuzz-every-decoder unchanged).
