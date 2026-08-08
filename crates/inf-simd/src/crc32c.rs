//! CRC32C (Castagnoli, reflected polynomial `0x82F63B78`) — the checksum of
//! the log spine's batch frames (milestone M2-S01, master plan §8.1) and,
//! later, `.ick` checkpoint sections and the MANIFEST.
//!
//! Three implementations, one contract:
//! - x86-64: the SSE4.2 `crc32` instruction over 8-byte words, behind cached
//!   runtime detection (same dispatch pattern as `crlf.rs`).
//! - aarch64: the ARMv8 CRC extension (`__crc32cd`), behind runtime
//!   detection.
//! - portable: slicing-by-8 with const-built tables — the sim/dev tier and
//!   the property-test oracle for both hardware paths.
//!
//! The streaming form ([`crc32c_update`]) is the primitive: frames are
//! checksummed header+body in place, and later consumers (checkpoint
//! sections) stream. `crc32c(data) == crc32c_update(0, data)`.

#[cfg(target_arch = "x86_64")]
use std::sync::atomic::{AtomicU8, Ordering};

/// CRC32C of `data` (state-in/state-out convention: `crc32c_update(0, data)`).
///
/// Check value: `crc32c(b"123456789") == 0xE306_9283` (iSCSI/RFC 3720).
#[inline]
#[must_use]
pub fn crc32c(data: &[u8]) -> u32 {
    crc32c_update(0, data)
}

/// Streaming CRC32C: extend `crc` (the finalized CRC of the bytes so far;
/// `0` for an empty prefix) with `data`. Splitting the input at any byte
/// boundary yields the same result as one shot.
#[inline]
#[must_use]
pub fn crc32c_update(crc: u32, data: &[u8]) -> u32 {
    #[cfg(target_arch = "x86_64")]
    {
        if sse42_available() {
            // SAFETY: `crc32c_sse42` requires SSE4.2, proven present by the
            // cached `is_x86_feature_detected!` probe above.
            return unsafe { crc32c_sse42(crc, data) };
        }
    }
    #[cfg(target_arch = "aarch64")]
    {
        if std::arch::is_aarch64_feature_detected!("crc") {
            // SAFETY: `crc32c_armv8` requires the CRC extension, proven
            // present by the runtime probe above.
            return unsafe { crc32c_armv8(crc, data) };
        }
    }
    scalar_crc32c_update(crc, data)
}

#[cfg(target_arch = "x86_64")]
const SSE42_UNKNOWN: u8 = 0;
#[cfg(target_arch = "x86_64")]
const SSE42_YES: u8 = 1;
#[cfg(target_arch = "x86_64")]
const SSE42_NO: u8 = 2;

#[cfg(target_arch = "x86_64")]
static SSE42_LEVEL: AtomicU8 = AtomicU8::new(SSE42_UNKNOWN);

#[cfg(target_arch = "x86_64")]
#[inline]
fn sse42_available() -> bool {
    match SSE42_LEVEL.load(Ordering::Relaxed) {
        SSE42_YES => true,
        SSE42_NO => false,
        _ => {
            let detected = std::arch::is_x86_feature_detected!("sse4.2");
            SSE42_LEVEL.store(if detected { SSE42_YES } else { SSE42_NO }, Ordering::Relaxed);
            detected
        }
    }
}

/// SSE4.2 path: `crc32q` over 8-byte little-endian words, byte tail.
///
/// No pointer arithmetic — `chunks_exact` + `from_le_bytes` keep every load
/// bounds-checked by construction; the only unsafety is the `target_feature`
/// contract (see SAFETY.md).
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse4.2")]
unsafe fn crc32c_sse42(crc: u32, data: &[u8]) -> u32 {
    use core::arch::x86_64::{_mm_crc32_u8, _mm_crc32_u64};

    let mut state = u64::from(!crc);
    let mut chunks = data.chunks_exact(8);
    for chunk in &mut chunks {
        let word = u64::from_le_bytes(chunk.try_into().expect("chunks_exact(8) yields 8 bytes"));
        state = _mm_crc32_u64(state, word);
    }
    let mut state = state as u32;
    for &byte in chunks.remainder() {
        state = _mm_crc32_u8(state, byte);
    }
    !state
}

/// ARMv8 CRC-extension path: `crc32cx` over 8-byte words, byte tail.
/// Compile-gated for aarch64; verified by the same equivalence proptest as
/// the x86 path (see SAFETY.md for the verification status).
#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "crc")]
unsafe fn crc32c_armv8(crc: u32, data: &[u8]) -> u32 {
    use core::arch::aarch64::{__crc32cb, __crc32cd};

    let mut state = !crc;
    let mut chunks = data.chunks_exact(8);
    for chunk in &mut chunks {
        let word = u64::from_le_bytes(chunk.try_into().expect("chunks_exact(8) yields 8 bytes"));
        state = __crc32cd(state, word);
    }
    for &byte in chunks.remainder() {
        state = __crc32cb(state, byte);
    }
    !state
}

/// Slicing-by-8 tables: `TABLES[0]` is the classic bitwise byte table for the
/// reflected Castagnoli polynomial; `TABLES[k][b]` advances a CRC whose
/// lowest byte is `b` by `k` additional zero bytes.
const fn build_tables() -> [[u32; 256]; 8] {
    const POLY: u32 = 0x82F6_3B78;
    let mut tables = [[0u32; 256]; 8];
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ POLY } else { crc >> 1 };
            bit += 1;
        }
        tables[0][i] = crc;
        i += 1;
    }
    let mut k = 1;
    while k < 8 {
        let mut i = 0;
        while i < 256 {
            let prev = tables[k - 1][i];
            tables[k][i] = (prev >> 8) ^ tables[0][(prev & 0xFF) as usize];
            i += 1;
        }
        k += 1;
    }
    tables
}

static TABLES: [[u32; 256]; 8] = build_tables();

/// Portable slicing-by-8 CRC32C — the software fallback for the sim/dev tier
/// and the oracle the hardware paths are property-tested against.
#[must_use]
pub fn scalar_crc32c_update(crc: u32, data: &[u8]) -> u32 {
    let mut state = !crc;
    let mut chunks = data.chunks_exact(8);
    for chunk in &mut chunks {
        let lo = u32::from_le_bytes(chunk[0..4].try_into().expect("4-byte slice")) ^ state;
        let hi = u32::from_le_bytes(chunk[4..8].try_into().expect("4-byte slice"));
        state = TABLES[7][(lo & 0xFF) as usize]
            ^ TABLES[6][((lo >> 8) & 0xFF) as usize]
            ^ TABLES[5][((lo >> 16) & 0xFF) as usize]
            ^ TABLES[4][(lo >> 24) as usize]
            ^ TABLES[3][(hi & 0xFF) as usize]
            ^ TABLES[2][((hi >> 8) & 0xFF) as usize]
            ^ TABLES[1][((hi >> 16) & 0xFF) as usize]
            ^ TABLES[0][(hi >> 24) as usize];
    }
    for &byte in chunks.remainder() {
        state = (state >> 8) ^ TABLES[0][((state ^ u32::from(byte)) & 0xFF) as usize];
    }
    !state
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    /// RFC 3720 (iSCSI) appendix B.4 test vectors.
    #[test]
    fn rfc3720_vectors() {
        assert_eq!(crc32c(b""), 0);
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
        assert_eq!(crc32c(&[0u8; 32]), 0x8A91_36AA);
        assert_eq!(crc32c(&[0xFFu8; 32]), 0x62A8_AB43);
        let ascending: Vec<u8> = (0u8..32).collect();
        assert_eq!(crc32c(&ascending), 0x46DD_794E);
        let descending: Vec<u8> = (0u8..32).rev().collect();
        assert_eq!(crc32c(&descending), 0x113F_DB5C);
    }

    #[test]
    fn scalar_matches_vectors() {
        assert_eq!(scalar_crc32c_update(0, b"123456789"), 0xE306_9283);
        assert_eq!(scalar_crc32c_update(0, &[0u8; 32]), 0x8A91_36AA);
    }

    proptest! {
        /// The dispatched (hardware where available) path must agree with
        /// the slicing-by-8 oracle on arbitrary inputs.
        #[test]
        fn hardware_matches_scalar(data in proptest::collection::vec(any::<u8>(), 0..4096)) {
            prop_assert_eq!(crc32c(&data), scalar_crc32c_update(0, &data));
        }

        /// Streaming at any split point equals one-shot.
        #[test]
        fn streaming_split_equals_one_shot(
            data in proptest::collection::vec(any::<u8>(), 0..2048),
            split in any::<prop::sample::Index>(),
        ) {
            let mid = split.index(data.len() + 1);
            let (a, b) = data.split_at(mid);
            let streamed = crc32c_update(crc32c_update(0, a), b);
            prop_assert_eq!(streamed, crc32c(&data));
            let scalar_streamed = scalar_crc32c_update(scalar_crc32c_update(0, a), b);
            prop_assert_eq!(scalar_streamed, crc32c(&data));
        }
    }
}
