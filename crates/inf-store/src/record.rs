//! Record format **v0** (M0-S13, master plan §7.2) — variable-size packed
//! records living in the cell's [`inf_alloc::Arena`].
//!
//! ```text
//! [0]      type:4 (high) | flags:4 (low)
//! [1]      klen: u8
//! [2..5]   vlen: u24 LE          (≤ 16 MiB − 1 inline; blob ext is M7)
//! [5..8]   version: u24 LE       (WATCH/lease/CAS epoch, wraps mod 2^24)
//! [8..13]  expire_at_ms: u40 LE  (present iff FLAG_TTL)
//! [..]     key bytes, then value bytes (packed, no padding)
//! ```
//!
//! **Recorded deviation from the §7.2 freeze sketch:** the sketch says
//! "8 B fixed" but lists fields summing to 72 bits (`version: u32`) — the
//! arithmetic never closed. v0 keeps the 8-byte header by narrowing
//! `version` to **u24**: the only cost is WATCH/CAS ABA after exactly 2^24
//! mutations of one key inside one optimistic window, which M4 can rule out
//! with a side-table if it ever matters. The 8-byte header is load-bearing
//! for L5: the canonical (16 B, 64 B) gate record is exactly 88 B — zero
//! size-class slack — putting amortized overhead at ~18.6 B/key
//! (8 + 8-byte slot ÷ 0.85 load factor), inside the §7.2 18–24 B budget.

use inf_foundation::time::Nanos;

/// Maximum key length (klen is a u8; longer keys are rejected at the
/// command layer — Redis allows 512 MB keys, the compat surface documents
/// this M0 bound).
pub const MAX_KEY_LEN: usize = u8::MAX as usize;
/// Maximum inline value length (vlen is a u24).
pub const MAX_VAL_LEN: usize = (1 << 24) - 1;

pub(crate) const HEADER_LEN: usize = 8;
pub(crate) const TTL_EXT_LEN: usize = 5;
const FLAG_TTL: u8 = 0b0001;
/// String was produced by a byte-surgery mutation (APPEND/SETRANGE) — drives
/// `OBJECT ENCODING`'s `raw` answer the way Redis's `sds` conversion does
/// (M1-S02; the value alone can't tell `embstr` from `raw`).
const FLAG_RAW: u8 = 0b0010;
/// u40 ms — ~34.8 years of deterministic-clock range. Deadlines beyond it
/// clamp here (recorded deviation: "effectively never expires"; the store
/// clamps at every deadline-conversion site so the writer assert is an
/// internal invariant, not an input panic — M1-S03 fix of a latent M0 bound
/// panic on ≥ 34.8-year TTLs).
pub(crate) const MAX_EXPIRE_MS: u64 = (1 << 40) - 1;
/// Versions live in 24 bits (see the module deviation note).
pub(crate) const VERSION_MASK: u32 = (1 << 24) - 1;

/// Value type, 4 bits in the header. M3 adds the collection types; the
/// registry of type tags is an L11 seam (record-type registry).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum TypeTag {
    String = 1,
}

impl TypeTag {
    fn from_bits(bits: u8) -> Option<TypeTag> {
        (bits == 1).then_some(TypeTag::String)
    }
}

/// Everything needed to size and write one record.
#[derive(Copy, Clone, Debug)]
pub(crate) struct RecordSpec<'a> {
    pub key: &'a [u8],
    pub value: &'a [u8],
    pub version: u32,
    /// Absolute deadline on the injected clock, milliseconds.
    pub expire_at_ms: Option<u64>,
    /// Carries [`FLAG_RAW`] (`OBJECT ENCODING` honesty, M1-S02).
    pub raw: bool,
}

impl RecordSpec<'_> {
    /// Total bytes this record occupies in the arena.
    #[inline]
    pub fn encoded_len(&self) -> usize {
        HEADER_LEN
            + if self.expire_at_ms.is_some() { TTL_EXT_LEN } else { 0 }
            + self.key.len()
            + self.value.len()
    }

    /// Serializes into `buf` (exactly [`encoded_len`](Self::encoded_len) bytes).
    ///
    /// # Panics
    /// Panics on key/value/expiry bounds violations — the command layer
    /// validates inputs before reaching the record writer.
    pub fn write(&self, buf: &mut [u8]) {
        assert!(self.key.len() <= MAX_KEY_LEN, "key exceeds u8 length");
        assert!(self.value.len() <= MAX_VAL_LEN, "value exceeds u24 length");
        assert_eq!(buf.len(), self.encoded_len(), "buffer must be exact");
        let flags = if self.expire_at_ms.is_some() { FLAG_TTL } else { 0 }
            | if self.raw { FLAG_RAW } else { 0 };
        buf[0] = ((TypeTag::String as u8) << 4) | flags;
        buf[1] = self.key.len() as u8;
        let vlen = (self.value.len() as u32).to_le_bytes();
        buf[2..5].copy_from_slice(&vlen[..3]);
        let version = (self.version & VERSION_MASK).to_le_bytes();
        buf[5..8].copy_from_slice(&version[..3]);
        let mut at = HEADER_LEN;
        if let Some(ms) = self.expire_at_ms {
            assert!(ms <= MAX_EXPIRE_MS, "expiry exceeds u40 ms");
            buf[at..at + TTL_EXT_LEN].copy_from_slice(&ms.to_le_bytes()[..TTL_EXT_LEN]);
            at += TTL_EXT_LEN;
        }
        buf[at..at + self.key.len()].copy_from_slice(self.key);
        at += self.key.len();
        buf[at..].copy_from_slice(self.value);
    }
}

/// Computes a record's full encoded length from its fixed header alone —
/// how the store sizes the second arena read (header first, then the whole
/// record).
#[inline]
pub(crate) fn encoded_len_from_header(head: &[u8]) -> usize {
    debug_assert!(head.len() >= HEADER_LEN);
    let has_ttl = head[0] & FLAG_TTL != 0;
    let klen = head[1] as usize;
    let mut raw = [0u8; 4];
    raw[..3].copy_from_slice(&head[2..5]);
    let vlen = u32::from_le_bytes(raw) as usize;
    HEADER_LEN + if has_ttl { TTL_EXT_LEN } else { 0 } + klen + vlen
}

/// Borrowed view over an encoded record. Constructing one only reads the
/// fixed header; key/value slicing is lazy.
#[derive(Copy, Clone)]
pub(crate) struct RecordView<'a> {
    bytes: &'a [u8],
}

impl<'a> RecordView<'a> {
    /// Wraps the record at the start of `bytes` (the full allocation slice).
    ///
    /// # Panics
    /// Debug-panics if the header is malformed — records are written only by
    /// this module, so corruption here is an arena-lifecycle bug.
    #[inline]
    pub fn new(bytes: &'a [u8]) -> RecordView<'a> {
        debug_assert!(bytes.len() >= HEADER_LEN);
        debug_assert!(TypeTag::from_bits(bytes[0] >> 4).is_some(), "unknown type tag");
        RecordView { bytes }
    }

    #[inline]
    pub fn type_tag(self) -> TypeTag {
        TypeTag::from_bits(self.bytes[0] >> 4).expect("validated in new")
    }

    #[inline]
    fn has_ttl(self) -> bool {
        self.bytes[0] & FLAG_TTL != 0
    }

    /// True when the value was produced by byte surgery (APPEND/SETRANGE).
    #[inline]
    pub fn is_raw(self) -> bool {
        self.bytes[0] & FLAG_RAW != 0
    }

    #[inline]
    pub fn klen(self) -> usize {
        self.bytes[1] as usize
    }

    #[inline]
    pub fn vlen(self) -> usize {
        let mut raw = [0u8; 4];
        raw[..3].copy_from_slice(&self.bytes[2..5]);
        u32::from_le_bytes(raw) as usize
    }

    #[inline]
    pub fn version(self) -> u32 {
        let mut raw = [0u8; 4];
        raw[..3].copy_from_slice(&self.bytes[5..8]);
        u32::from_le_bytes(raw)
    }

    /// Absolute expiry deadline in clock ms, if any.
    #[inline]
    pub fn expire_at_ms(self) -> Option<u64> {
        if !self.has_ttl() {
            return None;
        }
        let mut raw = [0u8; 8];
        raw[..TTL_EXT_LEN].copy_from_slice(&self.bytes[HEADER_LEN..HEADER_LEN + TTL_EXT_LEN]);
        Some(u64::from_le_bytes(raw))
    }

    /// True if expired at `now` (expire-on-read, L7-deterministic).
    #[inline]
    pub fn is_expired(self, now: Nanos) -> bool {
        self.expire_at_ms().is_some_and(|at| now.0 / 1_000_000 >= at)
    }

    #[inline]
    fn key_at(self) -> usize {
        HEADER_LEN + if self.has_ttl() { TTL_EXT_LEN } else { 0 }
    }

    #[inline]
    pub fn key(self) -> &'a [u8] {
        let at = self.key_at();
        &self.bytes[at..at + self.klen()]
    }

    #[inline]
    pub fn value(self) -> &'a [u8] {
        let at = self.key_at() + self.klen();
        &self.bytes[at..at + self.vlen()]
    }

    /// Total encoded length (== the arena allocation length).
    #[inline]
    pub fn encoded_len(self) -> usize {
        self.key_at() + self.klen() + self.vlen()
    }
}

impl core::fmt::Debug for RecordView<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RecordView")
            .field("type", &self.type_tag())
            .field("klen", &self.klen())
            .field("vlen", &self.vlen())
            .field("version", &self.version())
            .field("expire_at_ms", &self.expire_at_ms())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(spec: RecordSpec<'_>) -> Vec<u8> {
        let mut buf = vec![0u8; spec.encoded_len()];
        spec.write(&mut buf);
        buf
    }

    #[test]
    fn header_is_eight_bytes_plus_optional_ttl() {
        let plain =
            RecordSpec { key: b"k", value: b"v", version: 1, expire_at_ms: None, raw: false };
        assert_eq!(plain.encoded_len(), 8 + 1 + 1);
        let ttl =
            RecordSpec { key: b"k", value: b"v", version: 1, expire_at_ms: Some(5), raw: false };
        assert_eq!(ttl.encoded_len(), 8 + 5 + 1 + 1);
    }

    #[test]
    fn gate_corpus_record_is_exactly_88_bytes() {
        // (16 B key, 64 B value): 8 B header + 80 payload — zero class slack.
        let spec = RecordSpec {
            key: &[b'k'; 16],
            value: &[b'v'; 64],
            version: 1,
            expire_at_ms: None,
            raw: false,
        };
        assert_eq!(spec.encoded_len(), 88);
    }

    #[test]
    fn view_reads_back_every_field() {
        let spec = RecordSpec {
            key: b"user:{42}:cart",
            value: &[0xAB; 300],
            version: 0xAD_BEEF,
            expire_at_ms: Some(MAX_EXPIRE_MS),
            raw: false,
        };
        let buf = roundtrip(spec);
        let view = RecordView::new(&buf);
        assert_eq!(view.type_tag(), TypeTag::String);
        assert_eq!(view.key(), b"user:{42}:cart");
        assert_eq!(view.value(), &[0xAB; 300][..]);
        assert_eq!(view.version(), 0xAD_BEEF);
        assert_eq!(view.expire_at_ms(), Some(MAX_EXPIRE_MS));
        assert_eq!(view.encoded_len(), buf.len());
    }

    #[test]
    fn version_wraps_mod_2_pow_24() {
        let spec = RecordSpec {
            key: b"k",
            value: b"v",
            version: u32::MAX,
            expire_at_ms: None,
            raw: false,
        };
        let buf = roundtrip(spec);
        assert_eq!(RecordView::new(&buf).version(), VERSION_MASK);
    }

    #[test]
    fn expiry_is_inclusive_at_the_millisecond() {
        let spec =
            RecordSpec { key: b"k", value: b"", version: 0, expire_at_ms: Some(10), raw: false };
        let buf = roundtrip(spec);
        let view = RecordView::new(&buf);
        assert!(!view.is_expired(Nanos(9_999_999)));
        assert!(view.is_expired(Nanos(10_000_000)));
    }

    #[test]
    fn no_ttl_never_expires() {
        let spec =
            RecordSpec { key: b"k", value: b"v", version: 0, expire_at_ms: None, raw: false };
        let buf = roundtrip(spec);
        assert!(!RecordView::new(&buf).is_expired(Nanos(u64::MAX)));
    }

    #[test]
    fn empty_key_and_value_are_representable() {
        let spec = RecordSpec { key: b"", value: b"", version: 7, expire_at_ms: None, raw: false };
        let buf = roundtrip(spec);
        let view = RecordView::new(&buf);
        assert_eq!((view.key(), view.value(), view.version()), (&b""[..], &b""[..], 7));
    }

    #[test]
    fn max_bounds_roundtrip() {
        let key = vec![b'K'; MAX_KEY_LEN];
        let value = vec![b'V'; 1 << 16]; // representative large value
        let spec = RecordSpec {
            key: &key,
            value: &value,
            version: VERSION_MASK,
            expire_at_ms: None,
            raw: false,
        };
        let buf = roundtrip(spec);
        let view = RecordView::new(&buf);
        assert_eq!(view.klen(), MAX_KEY_LEN);
        assert_eq!(view.vlen(), 1 << 16);
        assert_eq!(view.key(), key.as_slice());
        assert_eq!(view.value(), value.as_slice());
    }
}
