//! Record format **v0** (M0-S13, master plan §7.2) — variable-size packed
//! records living in the cell's [`inf_alloc::Arena`].
//!
//! ```text
//! [0]      type:4 (high) | flags:4 (low)
//! [1]      klen: u8
//! [2..5]   vlen: u24 LE          (≤ 16 MiB − 1 inline; larger values
//!                                 store out of line — `StringExtent`
//!                                 carries a 24-byte extent reference as
//!                                 its value, M4-S17, ADR-0061 D2)
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
// The tier format's key window holds every record's key: the fixed
// header, the TTL extension and the longest key end inside it.
const _: () = assert!(HEADER_LEN + TTL_EXT_LEN + MAX_KEY_LEN <= inf_log::TIER_KEY_WINDOW_BYTES);
const FLAG_TTL: u8 = 0b0001;
/// String was produced by a byte-surgery mutation (APPEND/SETRANGE) — drives
/// `OBJECT ENCODING`'s `raw` answer the way Redis's `sds` conversion does
/// (M1-S02; the value alone can't tell `embstr` from `raw`).
const FLAG_RAW: u8 = 0b0010;
/// The last two spare flag bits hold a 2-bit CLOCK reference counter
/// (M1-S06): access saturates it to 3, the eviction hand decrements, a
/// record at 0 is an LRU victim. Living in the flags nibble costs zero
/// bytes per record — the L5 reason this is CLOCK and not timestamped LRU.
const REF_SHIFT: u32 = 2;
const REF_MASK: u8 = 0b1100;
/// u40 ms — ~34.8 years of deterministic-clock range. Deadlines beyond it
/// clamp here (recorded deviation: "effectively never expires"; the store
/// clamps at every deadline-conversion site so the writer assert is an
/// internal invariant, not an input panic — M1-S03 fix of a latent M0 bound
/// panic on ≥ 34.8-year TTLs). Public since ADR-0111: the command seam
/// saturates into this bound instead of refusing what Redis accepts.
pub const MAX_EXPIRE_MS: u64 = (1 << 40) - 1;

/// The store deadline for an internal-clock instant in milliseconds:
/// instants past [`MAX_EXPIRE_MS`] saturate to it (ADR-0111). Never
/// overflows `Nanos` — the bound times 10⁶ is far below `u64::MAX`.
#[inline]
pub fn saturating_deadline(internal_ms: u64) -> Nanos {
    Nanos::from_millis(internal_ms.min(MAX_EXPIRE_MS))
}
/// Versions live in 24 bits (see the module deviation note).
pub(crate) const VERSION_MASK: u32 = (1 << 24) - 1;

/// Value type, 4 bits in the header. M5 adds the collection types; the
/// registry of type tags is an L11 seam (record-type registry).
/// `JsonDoc = 2` was reserved by ADR-0032 D1 and is bound by ADR-0037;
/// `StringExtent = 3` is bound by ADR-0061 (the §7.2 sketch's "ext flag"
/// revised to a type tag — the flags nibble is fully allocated).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum TypeTag {
    String = 1,
    JsonDoc = 2,
    /// A string whose value lives out of line in a blob extent — the
    /// record's value bytes are the 24-byte [`ExtentRef`] (M4-S17).
    StringExtent = 3,
}

impl TypeTag {
    fn from_bits(bits: u8) -> Option<TypeTag> {
        match bits {
            1 => Some(TypeTag::String),
            2 => Some(TypeTag::JsonDoc),
            3 => Some(TypeTag::StringExtent),
            _ => None,
        }
    }
}

/// What kind of value a record holds — the type tag plus its type-specific
/// flag state. An enum so invalid combinations (a raw-flagged document)
/// are unrepresentable (ADR-0037 D1).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub(crate) enum RecordKind {
    /// Plain string; `raw` carries [`FLAG_RAW`] (`OBJECT ENCODING`
    /// honesty, M1-S02).
    String { raw: bool },
    /// A document (ADR-0037): value = form byte + inline tape / doc-arena
    /// handle.
    JsonDoc,
    /// A string stored out of line (ADR-0061 D2): value = 24-byte
    /// extent reference, never the value bytes.
    StringExtent,
}

impl RecordKind {
    #[inline]
    pub fn type_tag(self) -> TypeTag {
        match self {
            RecordKind::String { .. } => TypeTag::String,
            RecordKind::JsonDoc => TypeTag::JsonDoc,
            RecordKind::StringExtent => TypeTag::StringExtent,
        }
    }
}

/// The out-of-line value reference a [`TypeTag::StringExtent`] record
/// carries as its value bytes (master plan §7.2 "ext header"; frozen at
/// M4 exit — plan §3.2). `offset` is 0 in v1 (frozen wire space).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct ExtentRef {
    /// The blob extent holding the value (`blob-NNNNNN.iblob`).
    pub extent_id: u64,
    /// Byte offset of the value inside the extent (0 in v1, asserted).
    pub offset: u64,
    /// Exact value byte length.
    pub len: u64,
}

/// Encoded [`ExtentRef`] length — the `vlen` of every extent record.
pub const EXTENT_REF_LEN: usize = 24;

impl ExtentRef {
    /// Serializes into exactly [`EXTENT_REF_LEN`] bytes (LE, explicitly
    /// sized — this is record content and flows through WAL frames,
    /// checkpoint images, and tier files verbatim).
    #[inline]
    #[must_use]
    pub fn encode(&self) -> [u8; EXTENT_REF_LEN] {
        debug_assert_eq!(self.offset, 0, "v1 extent references start at offset 0");
        debug_assert!(self.len > 0, "an extent reference names at least one byte");
        let mut out = [0u8; EXTENT_REF_LEN];
        out[0..8].copy_from_slice(&self.extent_id.to_le_bytes());
        out[8..16].copy_from_slice(&self.offset.to_le_bytes());
        out[16..24].copy_from_slice(&self.len.to_le_bytes());
        out
    }

    /// Decodes a [`TypeTag::StringExtent`] record's value bytes.
    ///
    /// # Panics
    /// Panics when `value` is not exactly [`EXTENT_REF_LEN`] bytes or its
    /// offset is not 0 (ADR-0061 D2) — extent records are written only by
    /// this crate, so either is a record-lifecycle bug, not input.
    #[inline]
    #[must_use]
    pub fn decode(value: &[u8]) -> ExtentRef {
        assert_eq!(value.len(), EXTENT_REF_LEN, "extent reference is exactly 24 bytes");
        let offset = u64::from_le_bytes(value[8..16].try_into().expect("8 bytes"));
        // ADR-0061 D2, checked where the bytes come back in: a non-zero v1
        // offset is a lifecycle bug (or a CRC-passing corruption), not a
        // value the extent reader's bounds arithmetic should ever see.
        assert_eq!(offset, 0, "v1 extent references start at offset 0");
        ExtentRef {
            extent_id: u64::from_le_bytes(value[0..8].try_into().expect("8 bytes")),
            offset,
            len: u64::from_le_bytes(value[16..24].try_into().expect("8 bytes")),
        }
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
    pub kind: RecordKind,
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
        let raw = matches!(self.kind, RecordKind::String { raw: true });
        let flags =
            if self.expire_at_ms.is_some() { FLAG_TTL } else { 0 } | if raw { FLAG_RAW } else { 0 };
        buf[0] = ((self.kind.type_tag() as u8) << 4) | flags;
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

/// Saturates the in-place CLOCK reference counter of the record whose first
/// header byte is `flags` (access touch — one OR on a line the read already
/// owns; TTL/RAW bits untouched).
#[inline]
pub(crate) fn flags_ref_saturate(flags: u8) -> u8 {
    flags | REF_MASK
}

/// Marks a freshly-written record with one CLOCK generation (writes earn
/// modest recency; repeated READS saturate to 3 — the differential is what
/// lets the sweep tell a read-hot set from churn, M1-S06).
#[inline]
pub(crate) fn flags_ref_write(flags: u8) -> u8 {
    (flags & !REF_MASK) | (1 << REF_SHIFT)
}

/// Decrements the CLOCK reference counter (eviction-hand sweep).
#[inline]
pub(crate) fn flags_ref_decrement(flags: u8) -> u8 {
    let level = (flags & REF_MASK) >> REF_SHIFT;
    (flags & !REF_MASK) | (level.saturating_sub(1) << REF_SHIFT)
}

/// Bumps the u24 version field of an encoded record in place (wrapping) —
/// how handle-form document mutations version without a record rewrite
/// (ADR-0037 D4). `bytes` is the full record slice.
#[cfg(feature = "doc")]
#[inline]
pub(crate) fn bump_version_in_place(bytes: &mut [u8]) {
    debug_assert!(bytes.len() >= HEADER_LEN);
    let mut raw = [0u8; 4];
    raw[..3].copy_from_slice(&bytes[5..8]);
    let next = (u32::from_le_bytes(raw).wrapping_add(1) & VERSION_MASK).to_le_bytes();
    bytes[5..8].copy_from_slice(&next[..3]);
}

/// Decodes just the key from a record *prefix* — `Some` when the prefix
/// covers the fixed header, the TTL extension when present, and the whole
/// key; `None` when it is too short. The key always ends within
/// `HEADER_LEN + TTL_EXT_LEN + MAX_KEY_LEN` = 268 bytes of the record's
/// start, so a cold-read first window always holds it (review of
/// 2026-08-30, C2: `SCAN`'s cold key resolution must not require the
/// value's bytes to name a key).
#[inline]
pub(crate) fn key_from_prefix(bytes: &[u8]) -> Option<&[u8]> {
    if bytes.len() < HEADER_LEN {
        return None;
    }
    let klen = bytes[1] as usize;
    let at = HEADER_LEN + if bytes[0] & FLAG_TTL != 0 { TTL_EXT_LEN } else { 0 };
    bytes.get(at..at + klen)
}

/// A cold record's identity as a boot settle may use it (ADR-0174 D3;
/// the settle read's `parse` step): parsed from the record's **key
/// window** — its first `TieredTable::KEY_PREFIX_LEN` bytes, or fewer at
/// its file's end — by the one constructor that checks, in order, that
/// the window holds the header, that the type tag decodes, that the key
/// is whole, that the record's encoded length lies inside its file
/// (`left`, the bytes from the record's address to the file's end) and
/// that **the key hashes to the slot's hash**. A boot settle keeps or
/// removes a slot only on one of these: a frame checksum checks bytes,
/// not identity, and "distinct key" is answered only for a verified
/// record of another key.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ColdKey<'a> {
    key: &'a [u8],
    record_len: u32,
    kind: TypeTag,
    hash: u64,
}

/// Why a key window did not parse into a [`ColdKey`] — each a typed boot
/// refusal naming the check, never "distinct".
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ColdKeyError {
    /// Fewer bytes than the fixed header.
    ShortHeader { len: usize },
    /// The type tag's bits name no record type.
    TypeTag { bits: u8 },
    /// The window ends inside the key (or the header's TTL extension).
    KeyTruncated { len: usize },
    /// The record's encoded length runs past its file's end.
    LengthPastFile { record_len: u64, left: u64 },
    /// The key does not hash to the slot's hash: another key's record,
    /// misplaced or misdirected, under valid frame checksums.
    HashMismatch { slot: u64, key: u64 },
}

impl core::fmt::Display for ColdKeyError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ColdKeyError::ShortHeader { len } => {
                write!(f, "key window of {len} bytes holds no record header")
            }
            ColdKeyError::TypeTag { bits } => write!(f, "record type tag {bits} is unknown"),
            ColdKeyError::KeyTruncated { len } => {
                write!(f, "key window of {len} bytes ends inside the key")
            }
            ColdKeyError::LengthPastFile { record_len, left } => {
                write!(f, "record length {record_len} runs past the file's end ({left} bytes left)")
            }
            ColdKeyError::HashMismatch { slot, key } => {
                write!(f, "the record's key hashes to {key:#018x}, the slot to {slot:#018x}")
            }
        }
    }
}

impl std::error::Error for ColdKeyError {}

impl<'a> ColdKey<'a> {
    /// The one constructor (the checks in the type's documentation, in
    /// that order). `hash_of` is the namespace's keyed hash (ADR-0094).
    /// Total over arbitrary bytes: every refusal is a [`ColdKeyError`].
    // ADR-0144 D2/D3: a decoder scope; docs/lint-scopes.tsv names its tier per lint family.
    #[cfg_attr(
        not(test),
        deny(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_possible_wrap,
            clippy::arithmetic_side_effects
        )
    )]
    pub fn from_window(
        window: &'a [u8],
        left: u64,
        slot_hash: u64,
        hash_of: impl FnOnce(&[u8]) -> u64,
    ) -> Result<ColdKey<'a>, ColdKeyError> {
        if window.len() < HEADER_LEN {
            return Err(ColdKeyError::ShortHeader { len: window.len() });
        }
        let bits = window[0] >> 4;
        let kind = match TypeTag::from_bits(bits) {
            Some(kind) => kind,
            // The planted canary (DRR FCR-STTIER-01 §6): a constructor
            // that checks nothing takes an unknown tag for a string.
            None if cfg!(inf_canary_replay_settle_unchecked) => TypeTag::String,
            None => return Err(ColdKeyError::TypeTag { bits }),
        };
        let Some(key) = key_from_prefix(window) else {
            return Err(ColdKeyError::KeyTruncated { len: window.len() });
        };
        let record_len = encoded_len_from_header(window);
        let record_len_u64 = u64::try_from(record_len)
            .map_err(|_| ColdKeyError::LengthPastFile { record_len: u64::MAX, left })?;
        if record_len_u64 > left && !cfg!(inf_canary_replay_settle_unchecked) {
            return Err(ColdKeyError::LengthPastFile { record_len: record_len_u64, left });
        }
        let key_hash = hash_of(key);
        if key_hash != slot_hash && !cfg!(inf_canary_replay_settle_unchecked) {
            return Err(ColdKeyError::HashMismatch { slot: slot_hash, key: key_hash });
        }
        let record_len = u32::try_from(record_len)
            .map_err(|_| ColdKeyError::LengthPastFile { record_len: record_len_u64, left })?;
        Ok(ColdKey { key, record_len, kind, hash: slot_hash })
    }

    /// The verified key.
    #[inline]
    pub fn key(&self) -> &'a [u8] {
        self.key
    }

    /// The record's exact encoded length — its death's bytes.
    #[inline]
    pub fn record_len(&self) -> u32 {
        self.record_len
    }

    /// The record's type.
    #[inline]
    pub fn kind(&self) -> TypeTag {
        self.kind
    }

    /// The slot's hash the key verified against.
    #[inline]
    pub fn hash(&self) -> u64 {
        self.hash
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

    /// CLOCK reference level (0..=3) — eviction recency (M1-S06).
    #[inline]
    pub fn ref_level(self) -> u8 {
        (self.bytes[0] & REF_MASK) >> REF_SHIFT
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

    /// True if expired at `now` (expire-on-read, L7-deterministic). The
    /// deadline millisecond itself still serves the key — Redis's read
    /// path is `now > when` (`PTTL` answers 0 there); the record is gone
    /// from the next millisecond on (F-L05-05). One predicate serves
    /// reads, scans, checkpoints, eviction and the wheel's fire check, so
    /// the client view and the recovered view agree.
    #[inline]
    pub fn is_expired(self, now: Nanos) -> bool {
        self.expire_at_ms().is_some_and(|at| now.0 / 1_000_000 > at)
    }

    /// The record's kind: type tag plus type-specific flag state.
    #[inline]
    pub fn kind(self) -> RecordKind {
        match self.type_tag() {
            TypeTag::String => RecordKind::String { raw: self.is_raw() },
            TypeTag::JsonDoc => RecordKind::JsonDoc,
            TypeTag::StringExtent => RecordKind::StringExtent,
        }
    }

    #[inline]
    fn key_at(self) -> usize {
        HEADER_LEN + if self.has_ttl() { TTL_EXT_LEN } else { 0 }
    }

    /// Byte offset of the value region inside the record slice — where the
    /// document form byte and handle fields live for in-place patching
    /// (ADR-0037 D4).
    #[cfg(feature = "doc")]
    #[inline]
    pub fn value_offset(self) -> usize {
        self.key_at() + self.klen()
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
    use inf_foundation::KeyHasher;

    use super::*;

    /// ADR-0061 D2: `offset` is 0 in v1 and asserted so — on the decode
    /// side too (L04 style row), where a CRC-passing corrupt reference
    /// would otherwise reach the extent reader's bounds arithmetic.
    #[test]
    #[should_panic(expected = "v1 extent references start at offset 0")]
    fn decode_refuses_a_non_zero_v1_offset() {
        let mut bytes = ExtentRef { extent_id: 7, offset: 0, len: 16 }.encode();
        bytes[8] = 1;
        let _ = ExtentRef::decode(&bytes);
    }

    fn roundtrip(spec: RecordSpec<'_>) -> Vec<u8> {
        let mut buf = vec![0u8; spec.encoded_len()];
        spec.write(&mut buf);
        buf
    }

    #[test]
    fn header_is_eight_bytes_plus_optional_ttl() {
        let plain = RecordSpec {
            key: b"k",
            value: b"v",
            version: 1,
            expire_at_ms: None,
            kind: RecordKind::String { raw: false },
        };
        assert_eq!(plain.encoded_len(), 8 + 1 + 1);
        let ttl = RecordSpec {
            key: b"k",
            value: b"v",
            version: 1,
            expire_at_ms: Some(5),
            kind: RecordKind::String { raw: false },
        };
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
            kind: RecordKind::String { raw: false },
        };
        assert_eq!(spec.encoded_len(), 88);
    }

    /// Review of 2026-08-30 (C2): the key decodes from a record prefix —
    /// with and without the TTL extension, at the 255-byte key bound, and
    /// from a prefix holding none of the value — while a prefix that stops
    /// inside the key answers `None`, never a truncated key.
    #[test]
    fn key_decodes_from_a_value_free_prefix() {
        for expire_at_ms in [None, Some(5u64)] {
            let key = vec![b'K'; MAX_KEY_LEN];
            let spec = RecordSpec {
                key: &key,
                value: &[0xAB; 40_000],
                version: 1,
                expire_at_ms,
                kind: RecordKind::String { raw: false },
            };
            let buf = roundtrip(spec);
            let key_end =
                HEADER_LEN + if expire_at_ms.is_some() { TTL_EXT_LEN } else { 0 } + key.len();
            assert!(key_end <= 268, "prefix bound documented on key_from_prefix");
            assert_eq!(key_from_prefix(&buf[..key_end]), Some(&key[..]));
            assert_eq!(key_from_prefix(&buf), Some(&key[..]));
            assert_eq!(key_from_prefix(&buf[..key_end - 1]), None);
            assert_eq!(key_from_prefix(&buf[..HEADER_LEN - 1]), None);
        }
    }

    #[test]
    fn view_reads_back_every_field() {
        let spec = RecordSpec {
            key: b"user:{42}:cart",
            value: &[0xAB; 300],
            version: 0xAD_BEEF,
            expire_at_ms: Some(MAX_EXPIRE_MS),
            kind: RecordKind::String { raw: false },
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

    /// ADR-0174 D3: the one constructor refuses, in order, a window
    /// without a header, an unbound type tag, a key the window cuts, a
    /// length past the file and a key that does not hash to the slot's
    /// hash — and accepts a record shorter than the key window at its
    /// file's end, the window clamped to the file.
    #[test]
    fn cold_key_checks_in_order_and_accepts_a_clamped_window() {
        let hash_of = |key: &[u8]| KeyHasher::default().hash(key);
        let spec = RecordSpec {
            key: b"k",
            value: &[0xAB; 300],
            version: 1,
            expire_at_ms: Some(5),
            kind: RecordKind::String { raw: false },
        };
        let buf = roundtrip(spec);
        let slot = hash_of(b"k");
        let len = buf.len() as u64;
        assert_eq!(
            ColdKey::from_window(&buf[..HEADER_LEN - 1], len, slot, hash_of),
            Err(ColdKeyError::ShortHeader { len: HEADER_LEN - 1 })
        );
        let mut untagged = buf.clone();
        untagged[0] &= 0x0F;
        assert_eq!(
            ColdKey::from_window(&untagged, len, slot, hash_of),
            Err(ColdKeyError::TypeTag { bits: 0 })
        );
        let cut = HEADER_LEN + TTL_EXT_LEN; // the TTL extension, not the key
        assert_eq!(
            ColdKey::from_window(&buf[..cut], len, slot, hash_of),
            Err(ColdKeyError::KeyTruncated { len: cut })
        );
        assert_eq!(
            ColdKey::from_window(&buf, len - 1, slot, hash_of),
            Err(ColdKeyError::LengthPastFile { record_len: len, left: len - 1 })
        );
        let other = hash_of(b"other");
        assert_eq!(
            ColdKey::from_window(&buf, len, other, hash_of),
            Err(ColdKeyError::HashMismatch { slot: other, key: slot })
        );
        // The key window (268 B) is shorter than the record: the key is
        // whole, the length is the header's, the hash checks.
        let window = &buf[..HEADER_LEN + TTL_EXT_LEN + 1];
        let cold = ColdKey::from_window(window, len, slot, hash_of).expect("a verified key");
        assert_eq!(cold.key(), b"k");
        assert_eq!(u64::from(cold.record_len()), len);
        assert_eq!(cold.kind(), TypeTag::String);
        assert_eq!(cold.hash(), slot);
        // A record shorter than the key window at its file's end: the
        // whole record is the window and the file holds exactly it.
        let short = roundtrip(RecordSpec {
            key: b"k",
            value: b"v",
            version: 0,
            expire_at_ms: None,
            kind: RecordKind::String { raw: false },
        });
        let cold = ColdKey::from_window(&short, short.len() as u64, slot, hash_of)
            .expect("clamped to the file");
        assert_eq!(u64::from(cold.record_len()), short.len() as u64);
    }

    #[test]
    fn version_wraps_mod_2_pow_24() {
        let spec = RecordSpec {
            key: b"k",
            value: b"v",
            version: u32::MAX,
            expire_at_ms: None,
            kind: RecordKind::String { raw: false },
        };
        let buf = roundtrip(spec);
        assert_eq!(RecordView::new(&buf).version(), VERSION_MASK);
    }

    #[test]
    fn expiry_is_exclusive_at_the_deadline_millisecond() {
        let spec = RecordSpec {
            key: b"k",
            value: b"",
            version: 0,
            expire_at_ms: Some(10),
            kind: RecordKind::String { raw: false },
        };
        let buf = roundtrip(spec);
        let view = RecordView::new(&buf);
        assert!(!view.is_expired(Nanos(9_999_999)));
        assert!(!view.is_expired(Nanos(10_000_000)), "the deadline ms is served (Redis)");
        assert!(!view.is_expired(Nanos(10_999_999)));
        assert!(view.is_expired(Nanos(11_000_000)));
    }

    #[test]
    fn no_ttl_never_expires() {
        let spec = RecordSpec {
            key: b"k",
            value: b"v",
            version: 0,
            expire_at_ms: None,
            kind: RecordKind::String { raw: false },
        };
        let buf = roundtrip(spec);
        assert!(!RecordView::new(&buf).is_expired(Nanos(u64::MAX)));
    }

    #[test]
    fn empty_key_and_value_are_representable() {
        let spec = RecordSpec {
            key: b"",
            value: b"",
            version: 7,
            expire_at_ms: None,
            kind: RecordKind::String { raw: false },
        };
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
            kind: RecordKind::String { raw: false },
        };
        let buf = roundtrip(spec);
        let view = RecordView::new(&buf);
        assert_eq!(view.klen(), MAX_KEY_LEN);
        assert_eq!(view.vlen(), 1 << 16);
        assert_eq!(view.key(), key.as_slice());
        assert_eq!(view.value(), value.as_slice());
    }
}
