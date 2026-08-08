//! `idoc` header v1 (ADR-0036 D2): 8 bytes — magic `"iD"`, version, flags,
//! body length. Unknown versions and unknown/unsupported flag bits are
//! typed rejects, never skips: a document-bearing byte stream requires a
//! binary that speaks it (the §8.4 posture).

use crate::error::DocError;
use crate::limits::DOC_BYTES_MAX;

/// `"iD"` as little-endian u16 (bytes `0x69 0x44`).
pub const MAGIC: u16 = 0x4469;

/// Format version this binary writes and reads.
pub const VERSION: u8 = 1;

/// Flag bit 0: per-document key interning (defined by ADR-0036, built by
/// M3-S04 per ADR-0038). Accepted only when the `doc-intern-keys` feature
/// is compiled in; every other binary rejects it —
/// recognized-but-unsupported beats silently misreading.
pub const FLAG_INTERNED: u8 = 0b0000_0001;

/// Header length in bytes.
pub const HEADER_LEN: usize = 8;

/// Decoded header. `flags` is always 0 until S04 (rejection guarantees it).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Header {
    pub flags: u8,
    pub body_len: u32,
}

/// Append a v1 header. `body_len` is the exact root-value byte length.
/// The tape builders patch in place ([`patch`]); the interning transform
/// is the remaining append-style writer.
#[cfg(feature = "doc-intern-keys")]
pub fn encode(flags: u8, body_len: u32, out: &mut Vec<u8>) {
    debug_assert!(body_len as usize <= DOC_BYTES_MAX);
    debug_assert_eq!(flags & !FLAG_INTERNED, 0, "reserved flag bits must be zero");
    out.extend_from_slice(&MAGIC.to_le_bytes());
    out.push(VERSION);
    out.push(flags);
    out.extend_from_slice(&body_len.to_le_bytes());
}

/// Patch a v1 header over the 8-byte placeholder at `buf[..HEADER_LEN]` —
/// the builder/parser finish dance, allocation-free.
pub fn patch(buf: &mut [u8], flags: u8, body_len: u32) {
    debug_assert!(buf.len() >= HEADER_LEN);
    debug_assert!(body_len as usize <= DOC_BYTES_MAX);
    debug_assert_eq!(flags & !FLAG_INTERNED, 0, "reserved flag bits must be zero");
    buf[0..2].copy_from_slice(&MAGIC.to_le_bytes());
    buf[2] = VERSION;
    buf[3] = flags;
    buf[4..8].copy_from_slice(&body_len.to_le_bytes());
}

/// Decode and cross-check a header against the full document slice:
/// `body_len` must equal exactly the bytes that follow the header (a tape
/// never carries trailing garbage — L7's one-value-one-encoding at the
/// framing level).
pub fn decode(bytes: &[u8]) -> Result<Header, DocError> {
    if bytes.len() < HEADER_LEN {
        return Err(DocError::Truncated);
    }
    let magic = u16::from_le_bytes([bytes[0], bytes[1]]);
    if magic != MAGIC {
        return Err(DocError::BadMagic);
    }
    let version = bytes[2];
    if version != VERSION {
        return Err(DocError::UnsupportedVersion(version));
    }
    let flags = bytes[3];
    // Bit 0 is acceptable exactly when this binary can decode interned
    // tapes (ADR-0038 D4); reserved bits stay rejects forever.
    let supported = if cfg!(feature = "doc-intern-keys") { FLAG_INTERNED } else { 0 };
    if flags & !supported != 0 {
        return Err(DocError::UnsupportedFlags(flags));
    }
    let body_len = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
    if body_len as usize > DOC_BYTES_MAX {
        return Err(DocError::TooLarge { bytes: body_len as usize });
    }
    if bytes.len() - HEADER_LEN != body_len as usize {
        return Err(DocError::BadLength);
    }
    Ok(Header { flags, body_len })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(version: u8, flags: u8, body: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&MAGIC.to_le_bytes());
        v.push(version);
        v.push(flags);
        v.extend_from_slice(&(body.len() as u32).to_le_bytes());
        v.extend_from_slice(body);
        v
    }

    #[test]
    fn round_trip() {
        let bytes = doc(VERSION, 0, &[0xA0]);
        let h = decode(&bytes).expect("valid header");
        assert_eq!(h, Header { flags: 0, body_len: 1 });
    }

    #[test]
    fn rejections_are_typed() {
        assert_eq!(decode(&[]), Err(DocError::Truncated));
        assert_eq!(decode(&[0x69]), Err(DocError::Truncated));
        let mut bad_magic = doc(VERSION, 0, &[0xA0]);
        bad_magic[0] = b'X';
        assert_eq!(decode(&bad_magic), Err(DocError::BadMagic));
        assert_eq!(decode(&doc(2, 0, &[0xA0])), Err(DocError::UnsupportedVersion(2)));
        // Interned (bit0) is accepted only with the `doc-intern-keys`
        // feature (ADR-0038 D4); reserved bits reject forever.
        #[cfg(not(feature = "doc-intern-keys"))]
        assert_eq!(decode(&doc(VERSION, 0b1, &[0xA0])), Err(DocError::UnsupportedFlags(1)));
        #[cfg(feature = "doc-intern-keys")]
        assert_eq!(
            decode(&doc(VERSION, 0b1, &[0xA0])).map(|h| h.flags),
            Ok(FLAG_INTERNED),
            "bit0 header-decodes with the feature (body/table validation is TapeDoc's)"
        );
        assert_eq!(decode(&doc(VERSION, 0b10, &[0xA0])), Err(DocError::UnsupportedFlags(2)));
        assert_eq!(decode(&doc(VERSION, 0b11, &[0xA0])), Err(DocError::UnsupportedFlags(3)));
        // body_len must cover the tail exactly.
        let mut short = doc(VERSION, 0, &[0xA0]);
        short.push(0xA0);
        assert_eq!(decode(&short), Err(DocError::BadLength));
    }
}
