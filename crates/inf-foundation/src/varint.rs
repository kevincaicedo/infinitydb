//! LEB128 varints for codec framing.

/// Append the LEB128 encoding of `v` to `out`. At most 10 bytes.
#[inline]
pub fn encode_u64(mut v: u64, out: &mut Vec<u8>) {
    loop {
        let byte = (v & 0x7F) as u8;
        v >>= 7;
        if v == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// Decode a LEB128 u64 from the front of `buf`.
/// Returns `(value, bytes_consumed)`; `None` on truncation, > 10 bytes, or a
/// **non-minimal encoding**. Canonical form is required so decode→encode
/// round-trips byte-exact — the fabric codec and the log spine (L7, replay
/// digests) both depend on one-value-one-encoding. Found by the fabric_codec
/// fuzzer on the first Linux hour: `[0x80, 0x00]` decoded as 0 but
/// re-encoded as `[0x00]`.
#[inline]
pub fn decode_u64(buf: &[u8]) -> Option<(u64, usize)> {
    let mut value: u64 = 0;
    for (i, &byte) in buf.iter().enumerate().take(10) {
        value |= u64::from(byte & 0x7F) << (7 * i);
        if byte & 0x80 == 0 {
            // Reject non-canonical bits beyond 64 in the final byte.
            if i == 9 && byte > 0x01 {
                return None;
            }
            // Reject non-minimal encodings: a final 0x00 after a
            // continuation byte contributes no bits, so the canonical form
            // is always shorter.
            if i > 0 && byte == 0 {
                return None;
            }
            return Some((value, i + 1));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_edges() {
        for v in [0u64, 1, 127, 128, 16383, 16384, u32::MAX as u64, u64::MAX] {
            let mut buf = Vec::new();
            encode_u64(v, &mut buf);
            assert_eq!(decode_u64(&buf), Some((v, buf.len())), "value {v}");
        }
    }

    #[test]
    fn truncated_is_none() {
        let mut buf = Vec::new();
        encode_u64(u64::MAX, &mut buf);
        assert_eq!(decode_u64(&buf[..buf.len() - 1]), None);
        assert_eq!(decode_u64(&[]), None);
    }

    #[test]
    fn overlong_is_rejected() {
        // 11 continuation bytes can never be a valid u64.
        assert_eq!(decode_u64(&[0x80; 11]), None);
    }

    #[test]
    fn non_minimal_is_rejected() {
        // Fuzz regression: [0x80, 0x00] decoded as 0, re-encoded as [0x00].
        assert_eq!(decode_u64(&[0x80, 0x00]), None);
        assert_eq!(decode_u64(&[0x81, 0x00]), None);
        assert_eq!(decode_u64(&[0xFF, 0x80, 0x00]), None);
        // The shortest multi-byte canonical encodings stay accepted.
        assert_eq!(decode_u64(&[0x80, 0x01]), Some((128, 2)));
        assert_eq!(decode_u64(&[0xFF, 0x7F]), Some((16383, 2)));
    }
}
