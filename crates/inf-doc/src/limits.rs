//! Bounded everything (ADR-0036 D2/D6): the caps below are format law, not
//! tuning. M3-S07 makes both *per-namespace configurable downward* via the
//! M1 CONFIG classes; nothing may raise them past the format ceilings.

/// Maximum nesting depth (containers on the validation/build stack).
/// RedisJSON parity; a 129th nested container is a typed reject.
pub const DEPTH_MAX: usize = 128;

/// Document byte cap: 16 MiB − 1. This is a *ceiling built into field
/// widths*, not a checked constant: the store record `vlen` is u24 (§7.2)
/// and container skip-lengths are u24 (ADR-0036 D3), so a larger document
/// is unrepresentable. M4 blob extents lift where bytes live, not this.
pub const DOC_BYTES_MAX: usize = 0xFF_FFFF;

// The cap must fit the u24 skip-length fields — if this ever fails to
// compile, the format changed without its ADR.
const _: () = assert!(DOC_BYTES_MAX <= 0xFF_FFFF);
