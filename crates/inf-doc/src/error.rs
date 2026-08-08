//! Typed errors for the `idoc` format (ADR-0036). Operating conditions —
//! hostile bytes, depth, size, arena budget — are errors, never panics
//! (INFINITY_STYLE §Panics); builder *misuse* is a programmer error and
//! asserts instead.

use core::fmt;

/// Everything the format layer can refuse. Wire/command layers map these
/// to their own error vocabularies (`ERR document too large`, …) — the
/// format never speaks RESP (§3.3 boundary).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum DocError {
    /// Input ended before a complete header or value.
    Truncated,
    /// Header magic is not `"iD"`.
    BadMagic,
    /// Header names a version this binary does not speak (reject, never
    /// skip — the §8.4 posture applied to document bytes).
    UnsupportedVersion(u8),
    /// Header flags carry bits this binary does not implement (bit0 =
    /// interned is defined by ADR-0036 but rejected until M3-S04 lands;
    /// bits 1–7 are reserved).
    UnsupportedFlags(u8),
    /// Document exceeds the byte cap (16 MiB − 1; M3-S07 threads the
    /// per-namespace knob through the builder's `max_body`).
    TooLarge { bytes: usize },
    /// Nesting exceeds the depth cap (default 128, RedisJSON parity).
    DepthExceeded,
    /// Unknown or reserved tag byte.
    BadTag(u8),
    /// A length field disagrees with the bytes it claims to cover, or a
    /// value overruns its enclosing scope.
    BadLength,
    /// The encoding is valid-shaped but non-canonical (wrong-width string
    /// form, fixint-range i64 tag, non-minimal varint, non-finite f64) —
    /// one value, one encoding (L7).
    NonCanonical(&'static str),
    /// A string or key is not valid UTF-8.
    BadUtf8,
    /// An object key position holds a non-string, or an object closes
    /// mid-pair.
    BadKey,
    /// NaN/±Inf cannot be represented (RedisJSON model); producers error
    /// at the command layer, and the builder refuses the bits here.
    NonFiniteNumber,
    /// The per-cell document arena refused the allocation (budget) — the
    /// bounded-everything backpressure seam, not a crash.
    ArenaExhausted,
}

impl fmt::Display for DocError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DocError::Truncated => write!(f, "idoc truncated"),
            DocError::BadMagic => write!(f, "idoc bad magic"),
            DocError::UnsupportedVersion(v) => write!(f, "idoc unsupported version {v}"),
            DocError::UnsupportedFlags(bits) => {
                write!(f, "idoc unsupported flags {bits:#04x}")
            }
            DocError::TooLarge { bytes } => write!(f, "document too large ({bytes} bytes)"),
            DocError::DepthExceeded => write!(f, "document nesting too deep"),
            DocError::BadTag(t) => write!(f, "idoc unknown tag {t:#04x}"),
            DocError::BadLength => write!(f, "idoc length mismatch"),
            DocError::NonCanonical(what) => write!(f, "idoc non-canonical encoding: {what}"),
            DocError::BadUtf8 => write!(f, "idoc invalid utf-8"),
            DocError::BadKey => write!(f, "idoc bad object key"),
            DocError::NonFiniteNumber => write!(f, "non-finite number is unrepresentable"),
            DocError::ArenaExhausted => write!(f, "document arena budget exhausted"),
        }
    }
}

impl core::error::Error for DocError {}
