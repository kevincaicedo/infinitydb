//! CRC32C/Castagnoli kernel for M2 log frames.
//!
//! The public entry point uses hardware when the current CPU supports it and
//! otherwise falls back to the scalar table. The scalar function stays public
//! as the benchmark/oracle baseline required by M2-S01.

#[inline]
pub fn crc32c(data: &[u8]) -> u32 {
    let mut crc = Crc32c::new();
    crc.update(data);
    crc.finish()
}

#[inline]
pub fn scalar_crc32c(data: &[u8]) -> u32 {
    !scalar_crc32c_update(!0u32, data)
}

#[inline]
pub fn scalar_crc32c_update(mut state: u32, data: &[u8]) -> u32 {
    for &byte in data {
        let index = ((state ^ u32::from(byte)) & 0xFF) as usize;
        state = (state >> 8) ^ CRC32C_TABLE[index];
    }
    state
}

/// Incremental CRC32C/Castagnoli hasher.
///
/// The state is the unfinalized CRC state. `finish()` applies the final xor, so
/// feeding chunks in order is byte-equivalent to [`crc32c`] over their
/// concatenation.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Crc32c {
    state: u32,
}

impl Crc32c {
    #[inline]
    pub const fn new() -> Crc32c {
        Crc32c { state: !0u32 }
    }

    #[inline]
    pub fn update(&mut self, data: &[u8]) {
        self.state = imp::crc32c_update(self.state, data);
    }

    #[inline]
    pub const fn finish(self) -> u32 {
        !self.state
    }
}

impl Default for Crc32c {
    #[inline]
    fn default() -> Self {
        Crc32c::new()
    }
}

const fn build_crc32c_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0x82F6_3B78 } else { crc >> 1 };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}

static CRC32C_TABLE: [u32; 256] = build_crc32c_table();

#[cfg(target_arch = "x86_64")]
#[allow(unsafe_code)]
mod imp {
    use core::arch::x86_64::{_mm_crc32_u8, _mm_crc32_u64};
    use core::sync::atomic::{AtomicU8, Ordering};

    const UNKNOWN: u8 = 0;
    const SCALAR: u8 = 1;
    const SSE42: u8 = 2;

    static CRC32C_LEVEL: AtomicU8 = AtomicU8::new(UNKNOWN);

    #[inline]
    pub fn crc32c_update(state: u32, data: &[u8]) -> u32 {
        let mut level = CRC32C_LEVEL.load(Ordering::Relaxed);
        if level == UNKNOWN {
            level = if std::arch::is_x86_feature_detected!("sse4.2") { SSE42 } else { SCALAR };
            CRC32C_LEVEL.store(level, Ordering::Relaxed);
        }

        if level == SSE42 {
            // SAFETY: runtime detection above guarantees SSE4.2.
            unsafe { sse42_crc32c_update(state, data) }
        } else {
            super::scalar_crc32c_update(state, data)
        }
    }

    #[target_feature(enable = "sse4.2")]
    unsafe fn sse42_crc32c_update(state: u32, data: &[u8]) -> u32 {
        let mut crc = u64::from(state);
        let mut offset = 0usize;

        while offset + 8 <= data.len() {
            let word = u64::from_le_bytes([
                data[offset],
                data[offset + 1],
                data[offset + 2],
                data[offset + 3],
                data[offset + 4],
                data[offset + 5],
                data[offset + 6],
                data[offset + 7],
            ]);
            crc = _mm_crc32_u64(crc, word);
            offset += 8;
        }

        let mut crc32 = crc as u32;
        while offset < data.len() {
            crc32 = _mm_crc32_u8(crc32, data[offset]);
            offset += 1;
        }
        crc32
    }
}

#[cfg(target_arch = "aarch64")]
#[allow(unsafe_code)]
mod imp {
    use core::arch::aarch64::{__crc32cb, __crc32cd};
    use core::sync::atomic::{AtomicU8, Ordering};

    const UNKNOWN: u8 = 0;
    const SCALAR: u8 = 1;
    const CRC: u8 = 2;

    static CRC32C_LEVEL: AtomicU8 = AtomicU8::new(UNKNOWN);

    #[inline]
    pub fn crc32c_update(state: u32, data: &[u8]) -> u32 {
        let mut level = CRC32C_LEVEL.load(Ordering::Relaxed);
        if level == UNKNOWN {
            level = if std::arch::is_aarch64_feature_detected!("crc") { CRC } else { SCALAR };
            CRC32C_LEVEL.store(level, Ordering::Relaxed);
        }

        if level == CRC {
            // SAFETY: runtime detection above guarantees the CRC extension.
            unsafe { arm_crc32c_update(state, data) }
        } else {
            super::scalar_crc32c_update(state, data)
        }
    }

    #[target_feature(enable = "crc")]
    unsafe fn arm_crc32c_update(mut crc: u32, data: &[u8]) -> u32 {
        let mut offset = 0usize;

        while offset + 8 <= data.len() {
            let word = u64::from_le_bytes([
                data[offset],
                data[offset + 1],
                data[offset + 2],
                data[offset + 3],
                data[offset + 4],
                data[offset + 5],
                data[offset + 6],
                data[offset + 7],
            ]);
            crc = __crc32cd(crc, word);
            offset += 8;
        }

        while offset < data.len() {
            crc = __crc32cb(crc, data[offset]);
            offset += 1;
        }
        crc
    }
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
mod imp {
    #[inline]
    pub fn crc32c_update(state: u32, data: &[u8]) -> u32 {
        super::scalar_crc32c_update(state, data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn crc32c_reference_vector() {
        assert_eq!(scalar_crc32c(b""), 0x0000_0000);
        assert_eq!(scalar_crc32c(b"123456789"), 0xE306_9283);
        assert_eq!(crc32c(b""), 0x0000_0000);
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
    }

    proptest! {
        #[test]
        fn dispatch_matches_scalar(data in prop::collection::vec(any::<u8>(), 0..4096)) {
            prop_assert_eq!(crc32c(&data), scalar_crc32c(&data));
        }

        #[test]
        fn incremental_matches_one_shot(
            data in prop::collection::vec(any::<u8>(), 0..4096),
            splits in prop::collection::vec(0usize..4096, 0..32),
        ) {
            let mut points = splits;
            points.retain(|split| *split <= data.len());
            points.sort_unstable();
            points.dedup();

            let mut crc = Crc32c::new();
            let mut start = 0usize;
            for end in points {
                crc.update(&data[start..end]);
                start = end;
            }
            crc.update(&data[start..]);

            prop_assert_eq!(crc.finish(), crc32c(&data));
            prop_assert_eq!(crc.finish(), scalar_crc32c(&data));
        }
    }
}
