use core::fmt;

use inf_foundation::{CellId, hash64};
use inf_simd::{Crc32c, crc32c};

use crate::manifest::{CheckpointId, CheckpointRef};
use crate::{Lsn, MAX_FRAME_LEN, NamespaceId};

pub const CHECKPOINT_IMAGE_MAGIC: u32 = u32::from_le_bytes(*b"ICK1");
pub const CHECKPOINT_SECTION_MAGIC: u32 = u32::from_le_bytes(*b"ICS1");
pub const CHECKPOINT_FOOTER_MAGIC: u32 = u32::from_le_bytes(*b"ICF1");
pub const CHECKPOINT_IMAGE_VERSION: u16 = 1;
pub const CHECKPOINT_IMAGE_HEADER_FIXED_LEN: usize = 32;
pub const CHECKPOINT_IMAGE_HEADER_TRAILER_LEN: usize = 4;
pub const CHECKPOINT_IMAGE_HEADER_MIN_LEN: usize =
    CHECKPOINT_IMAGE_HEADER_FIXED_LEN + CHECKPOINT_IMAGE_HEADER_TRAILER_LEN;
pub const CHECKPOINT_SECTION_HEADER_LEN: usize = 28;
pub const CHECKPOINT_SECTION_TRAILER_LEN: usize = 4;
pub const CHECKPOINT_FOOTER_LEN: usize = 24;
pub const MAX_CHECKPOINT_HEADER_NAMESPACES: usize = 4096;
pub const MAX_CHECKPOINT_IMAGE_SECTIONS: u32 = 16_384;
pub const MAX_CHECKPOINT_SECTION_PAYLOAD_LEN: usize =
    MAX_FRAME_LEN - CHECKPOINT_SECTION_HEADER_LEN - CHECKPOINT_SECTION_TRAILER_LEN;
pub const CHECKPOINT_FOOTER_DIGEST_ALGORITHM: &str = "inf-hash64-v1";

const CHECKPOINT_DIGEST_SEED: u64 = 0x4943_4b5f_4449_4731;
const CHECKPOINT_SECTION_RESERVED: u16 = 0;

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct CheckpointHeader<'a> {
    cell: CellId,
    checkpoint: CheckpointRef,
    section_count: u32,
    namespaces: &'a [NamespaceId],
}

impl<'a> CheckpointHeader<'a> {
    pub fn new(
        cell: CellId,
        checkpoint: CheckpointRef,
        section_count: u32,
        namespaces: &'a [NamespaceId],
    ) -> Result<CheckpointHeader<'a>, CheckpointImageError> {
        validate_section_count(section_count)?;
        validate_namespaces(namespaces)?;
        Ok(CheckpointHeader { cell, checkpoint, section_count, namespaces })
    }

    #[inline]
    pub const fn cell(self) -> CellId {
        self.cell
    }

    #[inline]
    pub const fn checkpoint(self) -> CheckpointRef {
        self.checkpoint
    }

    #[inline]
    pub const fn section_count(self) -> u32 {
        self.section_count
    }

    #[inline]
    pub const fn namespaces(self) -> &'a [NamespaceId] {
        self.namespaces
    }

    #[inline]
    pub fn digest(self) -> CheckpointDigest {
        CheckpointDigest::from_parts(
            self.cell,
            self.checkpoint,
            self.section_count,
            self.namespaces.iter().copied(),
        )
    }

    #[inline]
    fn encoded_len(self) -> usize {
        CHECKPOINT_IMAGE_HEADER_FIXED_LEN
            + self.namespaces.len() * 4
            + CHECKPOINT_IMAGE_HEADER_TRAILER_LEN
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct DecodedCheckpointHeader<'a> {
    cell: CellId,
    checkpoint: CheckpointRef,
    section_count: u32,
    namespace_bytes: &'a [u8],
}

impl<'a> DecodedCheckpointHeader<'a> {
    #[inline]
    pub const fn cell(self) -> CellId {
        self.cell
    }

    #[inline]
    pub const fn checkpoint(self) -> CheckpointRef {
        self.checkpoint
    }

    #[inline]
    pub const fn section_count(self) -> u32 {
        self.section_count
    }

    #[inline]
    pub fn namespace_count(self) -> usize {
        self.namespace_bytes.len() / 4
    }

    #[inline]
    pub fn namespaces(self) -> CheckpointNamespaceIter<'a> {
        CheckpointNamespaceIter { bytes: self.namespace_bytes, offset: 0 }
    }

    #[inline]
    pub fn digest(self) -> CheckpointDigest {
        CheckpointDigest::from_parts(
            self.cell,
            self.checkpoint,
            self.section_count,
            self.namespaces(),
        )
    }
}

#[derive(Clone, Debug)]
pub struct CheckpointNamespaceIter<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl Iterator for CheckpointNamespaceIter<'_> {
    type Item = NamespaceId;

    fn next(&mut self) -> Option<Self::Item> {
        if self.offset == self.bytes.len() {
            return None;
        }
        let namespace = NamespaceId::new(read_u32(self.bytes, self.offset));
        self.offset += 4;
        Some(namespace)
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum CheckpointSectionKind {
    NamespaceCatalog,
    Index,
    Records,
}

impl CheckpointSectionKind {
    #[inline]
    pub const fn tag(self) -> u16 {
        match self {
            CheckpointSectionKind::NamespaceCatalog => 1,
            CheckpointSectionKind::Index => 2,
            CheckpointSectionKind::Records => 3,
        }
    }

    #[inline]
    pub const fn from_tag(tag: u16) -> Option<CheckpointSectionKind> {
        match tag {
            1 => Some(CheckpointSectionKind::NamespaceCatalog),
            2 => Some(CheckpointSectionKind::Index),
            3 => Some(CheckpointSectionKind::Records),
            _ => None,
        }
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct CheckpointSectionRef<'a> {
    ordinal: u32,
    kind: CheckpointSectionKind,
    payload: &'a [u8],
}

impl<'a> CheckpointSectionRef<'a> {
    pub fn new(
        ordinal: u32,
        kind: CheckpointSectionKind,
        payload: &'a [u8],
    ) -> Result<CheckpointSectionRef<'a>, CheckpointImageError> {
        if payload.len() > MAX_CHECKPOINT_SECTION_PAYLOAD_LEN {
            return Err(CheckpointImageError::SectionPayloadTooLarge {
                len: payload.len(),
                max_len: MAX_CHECKPOINT_SECTION_PAYLOAD_LEN,
            });
        }
        Ok(CheckpointSectionRef { ordinal, kind, payload })
    }

    #[inline]
    pub const fn ordinal(self) -> u32 {
        self.ordinal
    }

    #[inline]
    pub const fn kind(self) -> CheckpointSectionKind {
        self.kind
    }

    #[inline]
    pub const fn payload(self) -> &'a [u8] {
        self.payload
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct CheckpointSectionMeta {
    ordinal: u32,
    kind: CheckpointSectionKind,
    payload_len: u32,
    payload_crc: u32,
    section_crc: u32,
}

impl CheckpointSectionMeta {
    #[inline]
    pub const fn ordinal(self) -> u32 {
        self.ordinal
    }

    #[inline]
    pub const fn kind(self) -> CheckpointSectionKind {
        self.kind
    }

    #[inline]
    pub const fn payload_len(self) -> u32 {
        self.payload_len
    }

    #[inline]
    pub const fn payload_crc(self) -> u32 {
        self.payload_crc
    }

    #[inline]
    pub const fn section_crc(self) -> u32 {
        self.section_crc
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct DecodedCheckpointSectionHeader {
    ordinal: u32,
    kind: CheckpointSectionKind,
    payload_len: u32,
    payload_crc: u32,
}

impl DecodedCheckpointSectionHeader {
    #[inline]
    pub const fn ordinal(self) -> u32 {
        self.ordinal
    }

    #[inline]
    pub const fn kind(self) -> CheckpointSectionKind {
        self.kind
    }

    #[inline]
    pub const fn payload_len(self) -> u32 {
        self.payload_len
    }

    #[inline]
    pub const fn payload_crc(self) -> u32 {
        self.payload_crc
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct DecodedCheckpointSection<'a> {
    meta: CheckpointSectionMeta,
    payload: &'a [u8],
}

impl<'a> DecodedCheckpointSection<'a> {
    #[inline]
    pub const fn meta(self) -> CheckpointSectionMeta {
        self.meta
    }

    #[inline]
    pub const fn ordinal(self) -> u32 {
        self.meta.ordinal
    }

    #[inline]
    pub const fn kind(self) -> CheckpointSectionKind {
        self.meta.kind
    }

    #[inline]
    pub const fn payload(self) -> &'a [u8] {
        self.payload
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct CheckpointSectionFrameParts {
    header: [u8; CHECKPOINT_SECTION_HEADER_LEN],
    trailer: [u8; CHECKPOINT_SECTION_TRAILER_LEN],
    meta: CheckpointSectionMeta,
}

impl CheckpointSectionFrameParts {
    #[inline]
    pub fn header(&self) -> &[u8; CHECKPOINT_SECTION_HEADER_LEN] {
        &self.header
    }

    #[inline]
    pub fn trailer(&self) -> &[u8; CHECKPOINT_SECTION_TRAILER_LEN] {
        &self.trailer
    }

    #[inline]
    pub const fn meta(self) -> CheckpointSectionMeta {
        self.meta
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct CheckpointFooter {
    section_count: u32,
    digest: u64,
}

impl CheckpointFooter {
    #[inline]
    pub fn new(section_count: u32, digest: CheckpointDigest) -> CheckpointFooter {
        CheckpointFooter { section_count, digest: digest.finish() }
    }

    #[inline]
    pub const fn from_digest(section_count: u32, digest: u64) -> CheckpointFooter {
        CheckpointFooter { section_count, digest }
    }

    #[inline]
    pub const fn section_count(self) -> u32 {
        self.section_count
    }

    #[inline]
    pub const fn digest(self) -> u64 {
        self.digest
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct CheckpointDigest {
    state: u64,
}

impl CheckpointDigest {
    pub fn from_parts<I>(
        cell: CellId,
        checkpoint: CheckpointRef,
        section_count: u32,
        namespaces: I,
    ) -> CheckpointDigest
    where
        I: IntoIterator<Item = NamespaceId>,
    {
        let mut digest = CheckpointDigest { state: CHECKPOINT_DIGEST_SEED };
        digest.feed(b"ICK-DIGEST-v1");
        digest.feed_u16(cell.0);
        digest.feed_u16(0);
        digest.feed_u32(checkpoint.id().get());
        digest.feed_u32(checkpoint.begin_lsn().segment());
        digest.feed_u32(checkpoint.begin_lsn().offset());
        digest.feed_u32(section_count);
        let mut namespace_count = 0u32;
        for namespace in namespaces {
            digest.feed_u32(namespace.get());
            namespace_count = namespace_count.wrapping_add(1);
        }
        digest.feed_u32(namespace_count);
        digest
    }

    #[inline]
    pub fn update_section(&mut self, meta: CheckpointSectionMeta) {
        self.feed_u32(meta.ordinal);
        self.feed_u16(meta.kind.tag());
        self.feed_u16(CHECKPOINT_SECTION_RESERVED);
        self.feed_u32(meta.payload_len);
        self.feed_u32(meta.payload_crc);
        self.feed_u32(meta.section_crc);
    }

    #[inline]
    pub const fn finish(self) -> u64 {
        self.state
    }

    #[inline]
    fn feed(&mut self, bytes: &[u8]) {
        self.state = hash64(bytes, self.state);
    }

    #[inline]
    fn feed_u16(&mut self, value: u16) {
        self.feed(&value.to_le_bytes());
    }

    #[inline]
    fn feed_u32(&mut self, value: u32) {
        self.feed(&value.to_le_bytes());
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum CheckpointImageError {
    HeaderTooShort { len: usize, min_len: usize },
    HeaderTooLarge { len: usize, max_len: usize },
    HeaderLengthMismatch { expected: usize, got: usize },
    BadHeaderMagic { got: u32 },
    BadSectionMagic { got: u32 },
    BadFooterMagic { got: u32 },
    UnsupportedVersion { got: u16 },
    BadHeaderLen { got: u16 },
    BadSectionHeaderLen { got: u16 },
    BadFooterLen { got: u16 },
    BadReserved { got: u16 },
    BadHeaderCrc { expected: u32, got: u32 },
    BadSectionHeaderCrc { expected: u32, got: u32 },
    BadSectionPayloadCrc { expected: u32, got: u32 },
    BadSectionCrc { expected: u32, got: u32 },
    BadFooterCrc { expected: u32, got: u32 },
    InvalidCheckpointId { raw: u32 },
    SectionCountTooLarge { count: u32, max_count: u32 },
    NamespaceSetTooLarge { count: usize, max_count: usize },
    NamespaceSetUnsorted { previous: NamespaceId, current: NamespaceId },
    UnknownSectionKind { raw: u16 },
    SectionPayloadTooLarge { len: usize, max_len: usize },
    SectionTooShort { len: usize, min_len: usize },
    SectionLengthMismatch { expected: usize, got: usize },
    FooterLengthMismatch { expected: usize, got: usize },
    SectionCountMismatch { expected: u32, got: u32 },
    DigestMismatch { expected: u64, got: u64 },
}

impl fmt::Display for CheckpointImageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CheckpointImageError::HeaderTooShort { len, min_len } => {
                write!(f, "checkpoint header is {len} bytes, below minimum {min_len}")
            }
            CheckpointImageError::HeaderTooLarge { len, max_len } => {
                write!(f, "checkpoint header is {len} bytes, above max {max_len}")
            }
            CheckpointImageError::HeaderLengthMismatch { expected, got } => {
                write!(f, "checkpoint header is {got} bytes, expected {expected}")
            }
            CheckpointImageError::BadHeaderMagic { got } => {
                write!(f, "bad checkpoint header magic 0x{got:08x}")
            }
            CheckpointImageError::BadSectionMagic { got } => {
                write!(f, "bad checkpoint section magic 0x{got:08x}")
            }
            CheckpointImageError::BadFooterMagic { got } => {
                write!(f, "bad checkpoint footer magic 0x{got:08x}")
            }
            CheckpointImageError::UnsupportedVersion { got } => {
                write!(f, "unsupported checkpoint image version {got}")
            }
            CheckpointImageError::BadHeaderLen { got } => {
                write!(f, "bad checkpoint header length {got}")
            }
            CheckpointImageError::BadSectionHeaderLen { got } => {
                write!(f, "bad checkpoint section header length {got}")
            }
            CheckpointImageError::BadFooterLen { got } => {
                write!(f, "bad checkpoint footer length {got}")
            }
            CheckpointImageError::BadReserved { got } => {
                write!(f, "non-zero checkpoint reserved field {got}")
            }
            CheckpointImageError::BadHeaderCrc { expected, got } => write!(
                f,
                "bad checkpoint header crc32c: expected 0x{expected:08x}, got 0x{got:08x}"
            ),
            CheckpointImageError::BadSectionHeaderCrc { expected, got } => write!(
                f,
                "bad checkpoint section header crc32c: expected 0x{expected:08x}, got 0x{got:08x}"
            ),
            CheckpointImageError::BadSectionPayloadCrc { expected, got } => write!(
                f,
                "bad checkpoint section payload crc32c: expected 0x{expected:08x}, got 0x{got:08x}"
            ),
            CheckpointImageError::BadSectionCrc { expected, got } => write!(
                f,
                "bad checkpoint section crc32c: expected 0x{expected:08x}, got 0x{got:08x}"
            ),
            CheckpointImageError::BadFooterCrc { expected, got } => write!(
                f,
                "bad checkpoint footer crc32c: expected 0x{expected:08x}, got 0x{got:08x}"
            ),
            CheckpointImageError::InvalidCheckpointId { raw } => {
                write!(f, "checkpoint id {raw} exceeds v1 max")
            }
            CheckpointImageError::SectionCountTooLarge { count, max_count } => {
                write!(f, "checkpoint image has {count} sections, above max {max_count}")
            }
            CheckpointImageError::NamespaceSetTooLarge { count, max_count } => {
                write!(f, "checkpoint namespace set has {count} entries, above max {max_count}")
            }
            CheckpointImageError::NamespaceSetUnsorted { previous, current } => write!(
                f,
                "checkpoint namespace set is not strictly sorted: {previous:?} before {current:?}"
            ),
            CheckpointImageError::UnknownSectionKind { raw } => {
                write!(f, "unknown checkpoint section kind {raw}")
            }
            CheckpointImageError::SectionPayloadTooLarge { len, max_len } => {
                write!(f, "checkpoint section payload is {len} bytes, above max {max_len}")
            }
            CheckpointImageError::SectionTooShort { len, min_len } => {
                write!(f, "checkpoint section is {len} bytes, below minimum {min_len}")
            }
            CheckpointImageError::SectionLengthMismatch { expected, got } => {
                write!(f, "checkpoint section is {got} bytes, expected {expected}")
            }
            CheckpointImageError::FooterLengthMismatch { expected, got } => {
                write!(f, "checkpoint footer is {got} bytes, expected {expected}")
            }
            CheckpointImageError::SectionCountMismatch { expected, got } => {
                write!(f, "checkpoint footer section count {got} != expected {expected}")
            }
            CheckpointImageError::DigestMismatch { expected, got } => {
                write!(f, "checkpoint footer digest 0x{got:016x} != expected 0x{expected:016x}")
            }
        }
    }
}

impl std::error::Error for CheckpointImageError {}

pub fn encode_checkpoint_header(
    header: CheckpointHeader<'_>,
    out: &mut Vec<u8>,
) -> Result<(), CheckpointImageError> {
    validate_namespaces(header.namespaces)?;
    let encoded_len = header.encoded_len();
    if encoded_len > u16::MAX as usize {
        return Err(CheckpointImageError::HeaderTooLarge {
            len: encoded_len,
            max_len: u16::MAX as usize,
        });
    }

    out.clear();
    out.reserve_exact(encoded_len);
    put_u32(out, CHECKPOINT_IMAGE_MAGIC);
    put_u16(out, CHECKPOINT_IMAGE_VERSION);
    put_u16(out, encoded_len as u16);
    put_u16(out, header.cell.0);
    put_u16(out, 0);
    put_u32(out, header.checkpoint.id().get());
    put_u32(out, header.checkpoint.begin_lsn().segment());
    put_u32(out, header.checkpoint.begin_lsn().offset());
    put_u32(out, header.section_count);
    put_u32(out, header.namespaces.len() as u32);
    for namespace in header.namespaces {
        put_u32(out, namespace.get());
    }
    append_crc(out);
    debug_assert_eq!(out.len(), encoded_len);
    Ok(())
}

pub fn decode_checkpoint_header(
    bytes: &[u8],
) -> Result<DecodedCheckpointHeader<'_>, CheckpointImageError> {
    if bytes.len() < CHECKPOINT_IMAGE_HEADER_MIN_LEN {
        return Err(CheckpointImageError::HeaderTooShort {
            len: bytes.len(),
            min_len: CHECKPOINT_IMAGE_HEADER_MIN_LEN,
        });
    }
    let expected_len = checkpoint_header_len_from_prefix(bytes)?;
    if expected_len != bytes.len() {
        return Err(CheckpointImageError::HeaderLengthMismatch {
            expected: expected_len,
            got: bytes.len(),
        });
    }
    validate_header_crc(bytes)?;

    let raw_checkpoint = read_u32(bytes, 12);
    let id = CheckpointId::new(raw_checkpoint)
        .ok_or(CheckpointImageError::InvalidCheckpointId { raw: raw_checkpoint })?;
    let checkpoint = CheckpointRef::new(id, Lsn::new(read_u32(bytes, 16), read_u32(bytes, 20)));
    let namespace_bytes = &bytes
        [CHECKPOINT_IMAGE_HEADER_FIXED_LEN..bytes.len() - CHECKPOINT_IMAGE_HEADER_TRAILER_LEN];
    validate_namespace_bytes(namespace_bytes)?;

    Ok(DecodedCheckpointHeader {
        cell: CellId(read_u16(bytes, 8)),
        checkpoint,
        section_count: read_u32(bytes, 24),
        namespace_bytes,
    })
}

pub fn checkpoint_header_len_from_prefix(bytes: &[u8]) -> Result<usize, CheckpointImageError> {
    if bytes.len() < CHECKPOINT_IMAGE_HEADER_FIXED_LEN {
        return Err(CheckpointImageError::HeaderTooShort {
            len: bytes.len(),
            min_len: CHECKPOINT_IMAGE_HEADER_FIXED_LEN,
        });
    }
    let magic = read_u32(bytes, 0);
    if magic != CHECKPOINT_IMAGE_MAGIC {
        return Err(CheckpointImageError::BadHeaderMagic { got: magic });
    }
    validate_version(read_u16(bytes, 4))?;

    let header_len = read_u16(bytes, 6);
    let expected_len = header_len as usize;
    if expected_len < CHECKPOINT_IMAGE_HEADER_MIN_LEN {
        return Err(CheckpointImageError::BadHeaderLen { got: header_len });
    }
    if read_u16(bytes, 10) != 0 {
        return Err(CheckpointImageError::BadReserved { got: read_u16(bytes, 10) });
    }

    let namespace_count = read_u32(bytes, 28) as usize;
    let section_count = read_u32(bytes, 24);
    validate_section_count(section_count)?;
    if namespace_count > MAX_CHECKPOINT_HEADER_NAMESPACES {
        return Err(CheckpointImageError::NamespaceSetTooLarge {
            count: namespace_count,
            max_count: MAX_CHECKPOINT_HEADER_NAMESPACES,
        });
    }
    let expected_len = CHECKPOINT_IMAGE_HEADER_FIXED_LEN
        + namespace_count * 4
        + CHECKPOINT_IMAGE_HEADER_TRAILER_LEN;
    if expected_len != header_len as usize {
        return Err(CheckpointImageError::HeaderLengthMismatch {
            expected: expected_len,
            got: header_len as usize,
        });
    }
    Ok(expected_len)
}

pub fn encode_checkpoint_section(
    section: CheckpointSectionRef<'_>,
    out: &mut Vec<u8>,
) -> Result<CheckpointSectionMeta, CheckpointImageError> {
    let parts = encode_checkpoint_section_frame_parts(section)?;
    out.clear();
    out.reserve_exact(
        CHECKPOINT_SECTION_HEADER_LEN + section.payload.len() + CHECKPOINT_SECTION_TRAILER_LEN,
    );
    out.extend_from_slice(parts.header());
    out.extend_from_slice(section.payload);
    out.extend_from_slice(parts.trailer());
    Ok(parts.meta)
}

pub fn encode_checkpoint_section_frame_parts(
    section: CheckpointSectionRef<'_>,
) -> Result<CheckpointSectionFrameParts, CheckpointImageError> {
    if section.payload.len() > MAX_CHECKPOINT_SECTION_PAYLOAD_LEN {
        return Err(CheckpointImageError::SectionPayloadTooLarge {
            len: section.payload.len(),
            max_len: MAX_CHECKPOINT_SECTION_PAYLOAD_LEN,
        });
    }

    let payload_len = section.payload.len() as u32;
    let mut header = [0u8; CHECKPOINT_SECTION_HEADER_LEN];
    write_u32(&mut header, 0, CHECKPOINT_SECTION_MAGIC);
    write_u16(&mut header, 4, CHECKPOINT_IMAGE_VERSION);
    write_u16(&mut header, 6, CHECKPOINT_SECTION_HEADER_LEN as u16);
    write_u32(&mut header, 8, section.ordinal);
    write_u16(&mut header, 12, section.kind.tag());
    write_u16(&mut header, 14, CHECKPOINT_SECTION_RESERVED);
    write_u32(&mut header, 16, payload_len);
    let payload_crc = crc32c(section.payload);
    write_u32(&mut header, 20, payload_crc);
    let header_crc = crc32c(&header[..CHECKPOINT_SECTION_HEADER_LEN - 4]);
    write_u32(&mut header, 24, header_crc);
    let mut section_crc = Crc32c::new();
    section_crc.update(&header);
    section_crc.update(section.payload);
    let section_crc = section_crc.finish();
    let meta = CheckpointSectionMeta {
        ordinal: section.ordinal,
        kind: section.kind,
        payload_len,
        payload_crc,
        section_crc,
    };
    Ok(CheckpointSectionFrameParts { header, trailer: section_crc.to_le_bytes(), meta })
}

pub fn decode_checkpoint_section_frame_header(
    bytes: &[u8],
) -> Result<DecodedCheckpointSectionHeader, CheckpointImageError> {
    if bytes.len() < CHECKPOINT_SECTION_HEADER_LEN {
        return Err(CheckpointImageError::SectionTooShort {
            len: bytes.len(),
            min_len: CHECKPOINT_SECTION_HEADER_LEN,
        });
    }
    if bytes.len() != CHECKPOINT_SECTION_HEADER_LEN {
        return Err(CheckpointImageError::SectionLengthMismatch {
            expected: CHECKPOINT_SECTION_HEADER_LEN,
            got: bytes.len(),
        });
    }
    let magic = read_u32(bytes, 0);
    if magic != CHECKPOINT_SECTION_MAGIC {
        return Err(CheckpointImageError::BadSectionMagic { got: magic });
    }
    validate_version(read_u16(bytes, 4))?;
    let header_len = read_u16(bytes, 6);
    if header_len as usize != CHECKPOINT_SECTION_HEADER_LEN {
        return Err(CheckpointImageError::BadSectionHeaderLen { got: header_len });
    }
    validate_section_header_crc(bytes)?;
    let reserved = read_u16(bytes, 14);
    if reserved != CHECKPOINT_SECTION_RESERVED {
        return Err(CheckpointImageError::BadReserved { got: reserved });
    }
    let raw_kind = read_u16(bytes, 12);
    let kind = CheckpointSectionKind::from_tag(raw_kind)
        .ok_or(CheckpointImageError::UnknownSectionKind { raw: raw_kind })?;
    let payload_len = read_u32(bytes, 16);
    if payload_len as usize > MAX_CHECKPOINT_SECTION_PAYLOAD_LEN {
        return Err(CheckpointImageError::SectionPayloadTooLarge {
            len: payload_len as usize,
            max_len: MAX_CHECKPOINT_SECTION_PAYLOAD_LEN,
        });
    }

    Ok(DecodedCheckpointSectionHeader {
        ordinal: read_u32(bytes, 8),
        kind,
        payload_len,
        payload_crc: read_u32(bytes, 20),
    })
}

pub fn decode_checkpoint_section(
    bytes: &[u8],
) -> Result<DecodedCheckpointSection<'_>, CheckpointImageError> {
    if bytes.len() < CHECKPOINT_SECTION_HEADER_LEN + CHECKPOINT_SECTION_TRAILER_LEN {
        return Err(CheckpointImageError::SectionTooShort {
            len: bytes.len(),
            min_len: CHECKPOINT_SECTION_HEADER_LEN + CHECKPOINT_SECTION_TRAILER_LEN,
        });
    }
    let header = decode_checkpoint_section_frame_header(&bytes[..CHECKPOINT_SECTION_HEADER_LEN])?;
    let payload_len = header.payload_len as usize;
    let expected_len = CHECKPOINT_SECTION_HEADER_LEN + payload_len + CHECKPOINT_SECTION_TRAILER_LEN;
    if bytes.len() != expected_len {
        return Err(CheckpointImageError::SectionLengthMismatch {
            expected: expected_len,
            got: bytes.len(),
        });
    }

    let payload =
        &bytes[CHECKPOINT_SECTION_HEADER_LEN..CHECKPOINT_SECTION_HEADER_LEN + payload_len];
    let expected_payload_crc = crc32c(payload);
    let got_payload_crc = header.payload_crc;
    if got_payload_crc != expected_payload_crc {
        return Err(CheckpointImageError::BadSectionPayloadCrc {
            expected: expected_payload_crc,
            got: got_payload_crc,
        });
    }

    let section_crc_at = bytes.len() - CHECKPOINT_SECTION_TRAILER_LEN;
    let expected_section_crc = crc32c(&bytes[..section_crc_at]);
    let got_section_crc = read_u32(bytes, section_crc_at);
    if got_section_crc != expected_section_crc {
        return Err(CheckpointImageError::BadSectionCrc {
            expected: expected_section_crc,
            got: got_section_crc,
        });
    }

    Ok(DecodedCheckpointSection {
        meta: CheckpointSectionMeta {
            ordinal: header.ordinal,
            kind: header.kind,
            payload_len: payload_len as u32,
            payload_crc: got_payload_crc,
            section_crc: got_section_crc,
        },
        payload,
    })
}

pub fn encode_checkpoint_footer(footer: CheckpointFooter, out: &mut Vec<u8>) {
    out.clear();
    out.reserve_exact(CHECKPOINT_FOOTER_LEN);
    put_u32(out, CHECKPOINT_FOOTER_MAGIC);
    put_u16(out, CHECKPOINT_IMAGE_VERSION);
    put_u16(out, CHECKPOINT_FOOTER_LEN as u16);
    put_u32(out, footer.section_count);
    put_u64(out, footer.digest);
    append_crc(out);
    debug_assert_eq!(out.len(), CHECKPOINT_FOOTER_LEN);
}

pub fn decode_checkpoint_footer(bytes: &[u8]) -> Result<CheckpointFooter, CheckpointImageError> {
    if bytes.len() != CHECKPOINT_FOOTER_LEN {
        return Err(CheckpointImageError::FooterLengthMismatch {
            expected: CHECKPOINT_FOOTER_LEN,
            got: bytes.len(),
        });
    }
    let magic = read_u32(bytes, 0);
    if magic != CHECKPOINT_FOOTER_MAGIC {
        return Err(CheckpointImageError::BadFooterMagic { got: magic });
    }
    validate_version(read_u16(bytes, 4))?;
    let footer_len = read_u16(bytes, 6);
    if footer_len as usize != CHECKPOINT_FOOTER_LEN {
        return Err(CheckpointImageError::BadFooterLen { got: footer_len });
    }
    let crc_at = CHECKPOINT_FOOTER_LEN - 4;
    let expected_crc = crc32c(&bytes[..crc_at]);
    let got_crc = read_u32(bytes, crc_at);
    if got_crc != expected_crc {
        return Err(CheckpointImageError::BadFooterCrc { expected: expected_crc, got: got_crc });
    }
    Ok(CheckpointFooter { section_count: read_u32(bytes, 8), digest: read_u64(bytes, 12) })
}

pub fn validate_checkpoint_footer(
    footer: CheckpointFooter,
    expected_section_count: u32,
    digest: CheckpointDigest,
) -> Result<(), CheckpointImageError> {
    if footer.section_count != expected_section_count {
        return Err(CheckpointImageError::SectionCountMismatch {
            expected: expected_section_count,
            got: footer.section_count,
        });
    }
    let expected = digest.finish();
    if footer.digest != expected {
        return Err(CheckpointImageError::DigestMismatch { expected, got: footer.digest });
    }
    Ok(())
}

fn validate_version(version: u16) -> Result<(), CheckpointImageError> {
    if version != CHECKPOINT_IMAGE_VERSION {
        return Err(CheckpointImageError::UnsupportedVersion { got: version });
    }
    Ok(())
}

fn validate_section_count(count: u32) -> Result<(), CheckpointImageError> {
    if count > MAX_CHECKPOINT_IMAGE_SECTIONS {
        return Err(CheckpointImageError::SectionCountTooLarge {
            count,
            max_count: MAX_CHECKPOINT_IMAGE_SECTIONS,
        });
    }
    Ok(())
}

fn validate_namespaces(namespaces: &[NamespaceId]) -> Result<(), CheckpointImageError> {
    if namespaces.len() > MAX_CHECKPOINT_HEADER_NAMESPACES {
        return Err(CheckpointImageError::NamespaceSetTooLarge {
            count: namespaces.len(),
            max_count: MAX_CHECKPOINT_HEADER_NAMESPACES,
        });
    }
    for pair in namespaces.windows(2) {
        if pair[0].get() >= pair[1].get() {
            return Err(CheckpointImageError::NamespaceSetUnsorted {
                previous: pair[0],
                current: pair[1],
            });
        }
    }
    Ok(())
}

fn validate_namespace_bytes(bytes: &[u8]) -> Result<(), CheckpointImageError> {
    let mut previous: Option<NamespaceId> = None;
    for offset in (0..bytes.len()).step_by(4) {
        let current = NamespaceId::new(read_u32(bytes, offset));
        if let Some(previous) = previous
            && previous.get() >= current.get()
        {
            return Err(CheckpointImageError::NamespaceSetUnsorted { previous, current });
        }
        previous = Some(current);
    }
    Ok(())
}

fn validate_header_crc(bytes: &[u8]) -> Result<(), CheckpointImageError> {
    let crc_at = bytes.len() - CHECKPOINT_IMAGE_HEADER_TRAILER_LEN;
    let expected = crc32c(&bytes[..crc_at]);
    let got = read_u32(bytes, crc_at);
    if got != expected {
        return Err(CheckpointImageError::BadHeaderCrc { expected, got });
    }
    Ok(())
}

fn validate_section_header_crc(bytes: &[u8]) -> Result<(), CheckpointImageError> {
    let expected = crc32c(&bytes[..CHECKPOINT_SECTION_HEADER_LEN - 4]);
    let got = read_u32(bytes, CHECKPOINT_SECTION_HEADER_LEN - 4);
    if got != expected {
        return Err(CheckpointImageError::BadSectionHeaderCrc { expected, got });
    }
    Ok(())
}

fn append_crc(out: &mut Vec<u8>) {
    let crc = crc32c(out);
    put_u32(out, crc);
}

fn put_u16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn read_u16(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

fn read_u32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

fn read_u64(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes([
        bytes[at],
        bytes[at + 1],
        bytes[at + 2],
        bytes[at + 3],
        bytes[at + 4],
        bytes[at + 5],
        bytes[at + 6],
        bytes[at + 7],
    ])
}

fn write_u16(bytes: &mut [u8], at: usize, value: u16) {
    bytes[at..at + 2].copy_from_slice(&value.to_le_bytes());
}

fn write_u32(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn checkpoint_id(raw: u32) -> CheckpointId {
        CheckpointId::new(raw).unwrap()
    }

    fn header<'a>(namespaces: &'a [NamespaceId], section_count: u32) -> CheckpointHeader<'a> {
        CheckpointHeader::new(
            CellId(7),
            CheckpointRef::new(checkpoint_id(42), Lsn::new(3, 128)),
            section_count,
            namespaces,
        )
        .unwrap()
    }

    fn namespace_vec(raw: &[u32]) -> Vec<NamespaceId> {
        raw.iter().copied().map(NamespaceId::new).collect()
    }

    fn rewrite_header_crc(bytes: &mut Vec<u8>) {
        bytes.truncate(bytes.len() - CHECKPOINT_IMAGE_HEADER_TRAILER_LEN);
        append_crc(bytes);
    }

    fn rewrite_section_crcs(bytes: &mut Vec<u8>) {
        let payload = &bytes[CHECKPOINT_SECTION_HEADER_LEN..bytes.len() - 4];
        let payload_crc = crc32c(payload);
        bytes[20..24].copy_from_slice(&payload_crc.to_le_bytes());
        let header_crc = crc32c(&bytes[..CHECKPOINT_SECTION_HEADER_LEN - 4]);
        bytes[24..28].copy_from_slice(&header_crc.to_le_bytes());
        bytes.truncate(bytes.len() - 4);
        append_crc(bytes);
    }

    #[test]
    fn checkpoint_header_round_trips_namespace_set() {
        let namespaces = namespace_vec(&[1, 7, 19]);
        let expected = header(&namespaces, 2);
        let mut bytes = Vec::new();

        encode_checkpoint_header(expected, &mut bytes).unwrap();
        let got = decode_checkpoint_header(&bytes).unwrap();

        assert_eq!(got.cell(), CellId(7));
        assert_eq!(got.checkpoint(), expected.checkpoint());
        assert_eq!(got.section_count(), 2);
        assert_eq!(got.namespaces().collect::<Vec<_>>(), namespaces);
        assert_eq!(got.digest().finish(), expected.digest().finish());
    }

    #[test]
    fn checkpoint_header_prefix_reports_canonical_header_length() {
        let namespaces = namespace_vec(&[1, 7, 19]);
        let mut bytes = Vec::new();
        encode_checkpoint_header(header(&namespaces, 2), &mut bytes).unwrap();

        assert_eq!(
            checkpoint_header_len_from_prefix(&bytes[..CHECKPOINT_IMAGE_HEADER_FIXED_LEN]),
            Ok(bytes.len())
        );
    }

    #[test]
    fn checkpoint_header_rejects_unsorted_namespace_set() {
        let namespaces = namespace_vec(&[2, 2]);
        assert_eq!(
            CheckpointHeader::new(
                CellId(0),
                CheckpointRef::new(checkpoint_id(1), Lsn::new(0, 0)),
                0,
                &namespaces,
            ),
            Err(CheckpointImageError::NamespaceSetUnsorted {
                previous: NamespaceId::new(2),
                current: NamespaceId::new(2),
            })
        );
    }

    #[test]
    fn checkpoint_header_rejects_unbounded_section_count() {
        let namespaces = namespace_vec(&[1]);
        assert_eq!(
            CheckpointHeader::new(
                CellId(0),
                CheckpointRef::new(checkpoint_id(1), Lsn::new(0, 0)),
                MAX_CHECKPOINT_IMAGE_SECTIONS + 1,
                &namespaces,
            ),
            Err(CheckpointImageError::SectionCountTooLarge {
                count: MAX_CHECKPOINT_IMAGE_SECTIONS + 1,
                max_count: MAX_CHECKPOINT_IMAGE_SECTIONS,
            })
        );
    }

    #[test]
    fn checkpoint_header_rejects_bad_magic_version_crc_and_length() {
        let namespaces = namespace_vec(&[1, 2]);
        let mut bytes = Vec::new();
        encode_checkpoint_header(header(&namespaces, 1), &mut bytes).unwrap();

        let mut bad_magic = bytes.clone();
        bad_magic[0..4].copy_from_slice(&0u32.to_le_bytes());
        rewrite_header_crc(&mut bad_magic);
        assert_eq!(
            decode_checkpoint_header(&bad_magic),
            Err(CheckpointImageError::BadHeaderMagic { got: 0 })
        );

        let mut bad_version = bytes.clone();
        bad_version[4..6].copy_from_slice(&2u16.to_le_bytes());
        rewrite_header_crc(&mut bad_version);
        assert_eq!(
            decode_checkpoint_header(&bad_version),
            Err(CheckpointImageError::UnsupportedVersion { got: 2 })
        );

        let mut bad_crc = bytes.clone();
        bad_crc[16] ^= 0x80;
        assert!(matches!(
            decode_checkpoint_header(&bad_crc),
            Err(CheckpointImageError::BadHeaderCrc { .. })
        ));

        assert!(matches!(
            decode_checkpoint_header(&bytes[..bytes.len() - 1]),
            Err(CheckpointImageError::HeaderLengthMismatch { .. })
        ));
    }

    #[test]
    fn checkpoint_sections_round_trip_and_feed_digest() {
        let namespaces = namespace_vec(&[1]);
        let header = header(&namespaces, 2);
        let mut digest = header.digest();
        let mut bytes = Vec::new();

        let section = CheckpointSectionRef::new(
            0,
            CheckpointSectionKind::NamespaceCatalog,
            b"namespace-catalog",
        )
        .unwrap();
        let first = encode_checkpoint_section(section, &mut bytes).unwrap();
        let decoded = decode_checkpoint_section(&bytes).unwrap();
        assert_eq!(decoded.meta(), first);
        assert_eq!(decoded.payload(), b"namespace-catalog");
        digest.update_section(decoded.meta());

        let section =
            CheckpointSectionRef::new(1, CheckpointSectionKind::Records, b"records").unwrap();
        let second = encode_checkpoint_section(section, &mut bytes).unwrap();
        let decoded = decode_checkpoint_section(&bytes).unwrap();
        assert_eq!(decoded.meta(), second);
        assert_eq!(decoded.payload(), b"records");
        digest.update_section(decoded.meta());

        let footer = CheckpointFooter::new(2, digest);
        encode_checkpoint_footer(footer, &mut bytes);
        let decoded_footer = decode_checkpoint_footer(&bytes).unwrap();
        validate_checkpoint_footer(decoded_footer, 2, digest).unwrap();
    }

    #[test]
    fn checkpoint_section_frame_parts_match_whole_frame() {
        let section = CheckpointSectionRef::new(
            11,
            CheckpointSectionKind::Index,
            b"bounded-index-page-bytes",
        )
        .unwrap();
        let parts = encode_checkpoint_section_frame_parts(section).unwrap();
        let mut streaming = Vec::new();
        streaming.extend_from_slice(parts.header());
        streaming.extend_from_slice(section.payload());
        streaming.extend_from_slice(parts.trailer());

        let mut whole = Vec::new();
        let meta = encode_checkpoint_section(section, &mut whole).unwrap();

        assert_eq!(parts.meta(), meta);
        assert_eq!(streaming, whole);
        assert_eq!(decode_checkpoint_section(&streaming).unwrap().meta(), meta);
    }

    #[test]
    fn checkpoint_section_header_decodes_without_payload() {
        let section = CheckpointSectionRef::new(
            11,
            CheckpointSectionKind::Index,
            b"bounded-index-page-bytes",
        )
        .unwrap();
        let parts = encode_checkpoint_section_frame_parts(section).unwrap();
        let decoded = decode_checkpoint_section_frame_header(parts.header()).unwrap();

        assert_eq!(decoded.ordinal(), 11);
        assert_eq!(decoded.kind(), CheckpointSectionKind::Index);
        assert_eq!(decoded.payload_len(), b"bounded-index-page-bytes".len() as u32);
        assert_eq!(decoded.payload_crc(), parts.meta().payload_crc());
    }

    #[test]
    fn checkpoint_section_rejects_payload_and_frame_crc_mismatches() {
        let section =
            CheckpointSectionRef::new(7, CheckpointSectionKind::Index, b"payload").unwrap();
        let mut bytes = Vec::new();
        encode_checkpoint_section(section, &mut bytes).unwrap();

        let mut bad_payload = bytes.clone();
        bad_payload[CHECKPOINT_SECTION_HEADER_LEN] ^= 0x80;
        assert!(matches!(
            decode_checkpoint_section(&bad_payload),
            Err(CheckpointImageError::BadSectionPayloadCrc { .. })
                | Err(CheckpointImageError::BadSectionCrc { .. })
        ));

        let mut bad_frame_crc = bytes.clone();
        let last = bad_frame_crc.len() - 1;
        bad_frame_crc[last] ^= 0x80;
        assert!(matches!(
            decode_checkpoint_section(&bad_frame_crc),
            Err(CheckpointImageError::BadSectionCrc { .. })
        ));
    }

    #[test]
    fn checkpoint_section_rejects_unknown_kind_after_header_crc() {
        let section =
            CheckpointSectionRef::new(0, CheckpointSectionKind::Index, b"payload").unwrap();
        let mut bytes = Vec::new();
        encode_checkpoint_section(section, &mut bytes).unwrap();
        bytes[12..14].copy_from_slice(&99u16.to_le_bytes());
        rewrite_section_crcs(&mut bytes);

        assert_eq!(
            decode_checkpoint_section(&bytes),
            Err(CheckpointImageError::UnknownSectionKind { raw: 99 })
        );
    }

    #[test]
    fn checkpoint_footer_rejects_count_and_digest_mismatch() {
        let namespaces = namespace_vec(&[1]);
        let mut digest = header(&namespaces, 1).digest();
        let section =
            CheckpointSectionRef::new(0, CheckpointSectionKind::Records, b"records").unwrap();
        let mut bytes = Vec::new();
        let meta = encode_checkpoint_section(section, &mut bytes).unwrap();
        digest.update_section(meta);

        let footer = CheckpointFooter::new(1, digest);
        assert_eq!(
            validate_checkpoint_footer(footer, 2, digest),
            Err(CheckpointImageError::SectionCountMismatch { expected: 2, got: 1 })
        );

        let wrong_digest = CheckpointFooter::from_digest(1, footer.digest() ^ 1);
        assert_eq!(
            validate_checkpoint_footer(wrong_digest, 1, digest),
            Err(CheckpointImageError::DigestMismatch {
                expected: footer.digest(),
                got: footer.digest() ^ 1,
            })
        );
    }

    proptest! {
        #[test]
        fn checkpoint_section_payloads_round_trip(
            payloads in prop::collection::vec(prop::collection::vec(any::<u8>(), 0..2048), 0..16),
        ) {
            let namespaces = namespace_vec(&[1, 9, 27]);
            let header = header(&namespaces, payloads.len() as u32);
            let mut digest = header.digest();
            let mut bytes = Vec::new();

            for (ordinal, payload) in payloads.iter().enumerate() {
                let kind = match ordinal % 3 {
                    0 => CheckpointSectionKind::NamespaceCatalog,
                    1 => CheckpointSectionKind::Index,
                    _ => CheckpointSectionKind::Records,
                };
                let section = CheckpointSectionRef::new(ordinal as u32, kind, payload)?;
                let meta = encode_checkpoint_section(section, &mut bytes)?;
                let decoded = decode_checkpoint_section(&bytes)?;

                prop_assert_eq!(decoded.meta(), meta);
                prop_assert_eq!(decoded.ordinal(), ordinal as u32);
                prop_assert_eq!(decoded.kind(), kind);
                prop_assert_eq!(decoded.payload(), payload.as_slice());
                digest.update_section(decoded.meta());
            }

            let footer = CheckpointFooter::new(payloads.len() as u32, digest);
            encode_checkpoint_footer(footer, &mut bytes);
            let decoded_footer = decode_checkpoint_footer(&bytes)?;
            validate_checkpoint_footer(decoded_footer, payloads.len() as u32, digest)?;
        }
    }
}
