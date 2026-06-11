//! CRC16/XMODEM (poly 0x1021) and the Redis Cluster hash-tag rule.
//!
//! Test vectors are vendored from the Redis Cluster specification and
//! cross-checked against a live `redis-server` (`CLUSTER KEYSLOT`).

const fn build_table() -> [u16; 256] {
    let mut table = [0u16; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = (i as u16) << 8;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 0x8000 != 0 { (crc << 1) ^ 0x1021 } else { crc << 1 };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}

static TABLE: [u16; 256] = build_table();

/// CRC16/XMODEM as used by Redis Cluster key→slot mapping.
#[inline]
pub fn crc16(data: &[u8]) -> u16 {
    let mut crc: u16 = 0;
    for &byte in data {
        crc = (crc << 8) ^ TABLE[usize::from(((crc >> 8) ^ u16::from(byte)) & 0xFF)];
    }
    crc
}

/// Redis Cluster hash-tag extraction: if the key contains `{...}` with a
/// non-empty body, only the body is hashed; otherwise the whole key is.
#[inline]
pub fn hashtag(key: &[u8]) -> &[u8] {
    let Some(open) = key.iter().position(|&b| b == b'{') else {
        return key;
    };
    let rest = &key[open + 1..];
    let Some(close) = rest.iter().position(|&b| b == b'}') else {
        return key;
    };
    if close == 0 { key } else { &rest[..close] }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xmodem_reference_vector() {
        // Canonical CRC16/XMODEM check value (Redis Cluster spec appendix).
        assert_eq!(crc16(b"123456789"), 0x31C3);
        assert_eq!(crc16(b""), 0x0000);
    }

    #[test]
    fn redis_keyslot_vectors() {
        // Verified against redis-server 8.6.2: CLUSTER KEYSLOT <key>.
        assert_eq!(crc16(b"foo") % 16384, 12182);
        assert_eq!(crc16(b"bar") % 16384, 5061);
    }

    #[test]
    fn hashtag_rules() {
        // Spec examples: only the first {…} with non-empty body counts.
        assert_eq!(hashtag(b"{user1000}.following"), b"user1000");
        assert_eq!(hashtag(b"foo{}{bar}"), b"foo{}{bar}".as_slice()); // empty body: whole key
        assert_eq!(hashtag(b"foo{{bar}}zap"), b"{bar");
        assert_eq!(hashtag(b"foo{bar}{zap}"), b"bar");
        assert_eq!(hashtag(b"no-tag"), b"no-tag".as_slice());
        assert_eq!(hashtag(b"open{only"), b"open{only".as_slice());
    }
}
