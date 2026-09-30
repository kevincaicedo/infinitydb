//! Tape form: the durable, canonical `idoc` byte encoding (ADR-0036 D3).
//!
//! ```text
//! value := fixint | fixstr | null | false | true | i64 | f64
//!        | str8 | str24 | obj | arr
//! obj   := 0xA7 body_len:u24LE (key value)*      key := any string form
//! arr   := 0xA8 body_len:u24LE value*
//! ```
//!
//! Two properties are load-bearing:
//! - **Skip in O(1):** every value's extent is known from its tag plus a
//!   length field; `body_len` is the subtree skip distance — the parse-free
//!   traversal behind the `JSON.GET ≤ 1.5× GET` gate.
//! - **One value, one encoding (L7):** the validator rejects every
//!   non-canonical shape (wrong-width string form, fixint-range i64 tag,
//!   non-minimal varint, non-finite f64), so replay digests and the S23
//!   equivalence comparator can compare raw bytes.
//!
//! Validation runs **once at trust boundaries** (`TapeDoc::from_bytes`);
//! traversal trusts validated bytes (debug asserts only). The fuzz target
//! `idoc_decode` drives arbitrary bytes through here (L9).
// ADR-0144 D2/D3: a decoder scope; docs/lint-scopes.tsv names its tier per lint family.
#![cfg_attr(
    not(test),
    deny(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_possible_wrap,
        clippy::arithmetic_side_effects
    )
)]

use std::borrow::Cow;

use inf_foundation::varint;

use crate::error::DocError;
use crate::header::{FLAG_INTERNED, HEADER_LEN};
use crate::limits::DEPTH_MAX;

pub(crate) const TAG_NULL: u8 = 0xA0;
pub(crate) const TAG_FALSE: u8 = 0xA1;
pub(crate) const TAG_TRUE: u8 = 0xA2;
pub(crate) const TAG_I64: u8 = 0xA3;
pub(crate) const TAG_F64: u8 = 0xA4;
pub(crate) const TAG_STR8: u8 = 0xA5;
pub(crate) const TAG_STR24: u8 = 0xA6;
pub(crate) const TAG_OBJ: u8 = 0xA7;
pub(crate) const TAG_ARR: u8 = 0xA8;
/// Interned key reference: `0xA9 id:u16 LE`, object key position only —
/// allocated out of the ADR-0036 D3 reserved range by ADR-0038. Accepted
/// only on tapes whose header carries the interned flag (feature
/// `doc-intern-keys`); everywhere else it stays a reject.
pub(crate) const TAG_KEYREF: u8 = 0xA9;

pub(crate) const FIXSTR_BASE: u8 = 0x80;
pub(crate) const FIXSTR_MAX_LEN: usize = 31;
pub(crate) const STR8_MIN_LEN: usize = 32;
pub(crate) const STR24_MIN_LEN: usize = 256;
pub(crate) const FIXINT_MIN: i64 = -32;
pub(crate) const FIXINT_MAX: i64 = 127;
/// [`FIXINT_MIN`] as the signed tag byte a fixint is recognized by.
const FIXINT_MIN_TAG: i8 = -32;
const _: () = assert!(FIXINT_MIN_TAG as i64 == FIXINT_MIN, "the fixint tag floor is FIXINT_MIN");

/// Zigzag map for the `0xA3` i64 varint payload. Shift on the unsigned
/// re-interpretation: `i64::MIN << 1` would overflow the signed type.
#[inline]
pub(crate) fn zigzag(v: i64) -> u64 {
    (v.cast_unsigned() << 1) ^ (v >> 63).cast_unsigned()
}

#[inline]
pub(crate) fn unzigzag(u: u64) -> i64 {
    // The low bit selects an all-ones mask: 0 or -1, no overflow to guard.
    (u >> 1).cast_signed() ^ (u & 1).cast_signed().wrapping_neg()
}

#[inline]
#[allow(
    clippy::arithmetic_side_effects,
    reason = "bound: bytes[off] is indexed before either sum is evaluated, so off < \
              bytes.len() <= isize::MAX"
)]
pub(crate) fn read_u24(bytes: &[u8], off: usize) -> usize {
    debug_assert!(bytes.get(off..).is_some_and(|tail| tail.len() >= 3));
    bytes[off] as usize | (bytes[off + 1] as usize) << 8 | (bytes[off + 2] as usize) << 16
}

/// A validated string slice out of a tape or arena cell. UTF-8 was checked
/// at the trust boundary; comparisons stay memcmp (`as_bytes`), and the
/// `&str` view re-asserts the invariant instead of re-paying it per hop.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct DocStr<'a>(pub(crate) &'a [u8]);

impl<'a> DocStr<'a> {
    #[inline]
    pub fn as_bytes(&self) -> &'a [u8] {
        self.0
    }

    #[inline]
    pub fn to_str(&self) -> &'a str {
        str::from_utf8(self.0).expect("DocStr holds boundary-validated UTF-8 (ADR-0036 D6)")
    }
}

/// A decoded tape value. Containers borrow their exact body region, so
/// iteration needs no bounds beyond the slice itself.
#[derive(Copy, Clone, Debug)]
pub enum ValueRef<'a> {
    Null,
    Bool(bool),
    I64(i64),
    F64(f64),
    Str(DocStr<'a>),
    Obj(ObjRef<'a>),
    Arr(ArrRef<'a>),
}

/// The document's interned-key table handle (ADR-0038 D5). **Zero-sized
/// without the `doc-intern-keys` feature**: the S04 A/B measured the fat
/// pointer alone at ~4% on the depth-4 budget row, so default builds keep
/// the pre-S04 cursor layout byte-for-byte and only opt-in builds pay.
#[derive(Copy, Clone, Debug)]
pub struct Dict<'a> {
    #[cfg(feature = "doc-intern-keys")]
    table: &'a [u8],
    #[cfg(not(feature = "doc-intern-keys"))]
    _ghost: core::marker::PhantomData<&'a [u8]>,
}

impl<'a> Dict<'a> {
    /// The plain-tape dict: resolves nothing.
    #[inline]
    pub(crate) fn empty() -> Dict<'a> {
        Dict {
            #[cfg(feature = "doc-intern-keys")]
            table: &[],
            #[cfg(not(feature = "doc-intern-keys"))]
            _ghost: core::marker::PhantomData,
        }
    }

    #[cfg(feature = "doc-intern-keys")]
    #[inline]
    pub(crate) fn new(table: &'a [u8]) -> Dict<'a> {
        Dict { table }
    }

    /// The raw table region (empty on plain tapes).
    #[cfg(feature = "doc-intern-keys")]
    #[inline]
    pub(crate) fn region(&self) -> &'a [u8] {
        self.table
    }

    #[inline]
    fn is_empty(&self) -> bool {
        #[cfg(feature = "doc-intern-keys")]
        {
            self.table.is_empty()
        }
        #[cfg(not(feature = "doc-intern-keys"))]
        true
    }

    /// Resolve id → key bytes. O(id) walk — dicts are small and the hot
    /// lookup path resolves the *needle* once, then compares ids.
    fn get(&self, id: usize) -> Option<&'a [u8]> {
        #[cfg(feature = "doc-intern-keys")]
        {
            crate::intern::dict_region_get(self.table, id)
        }
        #[cfg(not(feature = "doc-intern-keys"))]
        {
            let _ = id;
            None
        }
    }

    /// Find a key's id (`None` on plain tapes or non-tabled keys).
    fn find(&self, key: &[u8]) -> Option<u16> {
        #[cfg(feature = "doc-intern-keys")]
        {
            crate::intern::dict_region_find(self.table, key)
        }
        #[cfg(not(feature = "doc-intern-keys"))]
        {
            let _ = key;
            None
        }
    }
}

/// Object view: `entries` is the exact `(key value)*` region; `dict` is
/// the document's interned-key table (empty on plain tapes — ADR-0038 D5).
#[derive(Copy, Clone, Debug)]
pub struct ObjRef<'a> {
    pub(crate) entries: &'a [u8],
    pub(crate) dict: Dict<'a>,
}

/// Array view: `body` is the exact `value*` region; `dict` propagates to
/// nested objects.
#[derive(Copy, Clone, Debug)]
pub struct ArrRef<'a> {
    pub(crate) body: &'a [u8],
    pub(crate) dict: Dict<'a>,
}

/// A validated document: header checked, body walked once (D6). Traversal
/// on this type trusts the bytes.
#[derive(Copy, Clone, Debug)]
pub struct TapeDoc<'a> {
    body: &'a [u8],
    /// Interned-key table (`count:u16 LE, count × (len:u16 LE, bytes)`).
    dict: Dict<'a>,
}

impl<'a> TapeDoc<'a> {
    /// Validate `bytes` as a complete v1 document (header + body walk).
    /// This is the trust boundary: replay, checkpoint load, and record
    /// admission call it; the read path never re-validates.
    pub fn from_bytes(bytes: &'a [u8]) -> Result<TapeDoc<'a>, DocError> {
        let header = crate::header::decode(bytes)?;
        let tail = &bytes[crate::header::HEADER_LEN..];
        if header.flags & crate::header::FLAG_INTERNED != 0 {
            #[cfg(feature = "doc-intern-keys")]
            {
                let (dict, body) = crate::intern::split_dict(tail)?;
                validate_body(body, dict)?;
                return Ok(TapeDoc { body, dict: Dict::new(dict) });
            }
            #[cfg(not(feature = "doc-intern-keys"))]
            unreachable!("header::decode rejects the interned flag without the feature");
        }
        validate_body(tail, &[])?;
        Ok(TapeDoc { body: tail, dict: Dict::empty() })
    }

    /// Wrap bytes that already crossed the trust boundary (store-owned
    /// record values, validated at ingest — ADR-0036 D6's model at the
    /// record layer, ADR-0037). Debug builds re-validate; release trusts.
    pub fn from_validated_bytes(bytes: &'a [u8]) -> TapeDoc<'a> {
        debug_assert!(
            TapeDoc::from_bytes(bytes).is_ok(),
            "from_validated_bytes requires boundary-validated bytes"
        );
        let flags = bytes[3];
        let tail = &bytes[crate::header::HEADER_LEN..];
        if flags & crate::header::FLAG_INTERNED != 0 {
            #[cfg(feature = "doc-intern-keys")]
            {
                let (dict, body) = crate::intern::split_dict(tail).expect("validated dict splits");
                return TapeDoc { body, dict: Dict::new(dict) };
            }
            #[cfg(not(feature = "doc-intern-keys"))]
            unreachable!("interned tapes cannot exist without the feature");
        }
        TapeDoc { body: tail, dict: Dict::empty() }
    }

    /// The root-value bytes (no header, no dict) — what fragment
    /// comparisons and the S17 `DocFull` payload framing operate on.
    /// Meaningful for comparison only on plain tapes (interned bytes never
    /// reach a comparator — ADR-0038 D3).
    #[inline]
    pub fn body(&self) -> &'a [u8] {
        self.body
    }

    /// The interned-key table region; empty on plain tapes.
    #[cfg(feature = "doc-intern-keys")]
    #[inline]
    pub fn dict(&self) -> &'a [u8] {
        self.dict.region()
    }

    #[inline]
    pub fn root(&self) -> ValueRef<'a> {
        let (value, next) = read_value(self.body, self.dict, 0);
        debug_assert_eq!(next, self.body.len(), "validated root spans the body exactly");
        value
    }

    /// The value starting at `offset` into [`Self::body`] — the
    /// `MatchResult::Popped` resolution surface (M3-S13, ADR-0042 D4).
    /// Offsets are meaningful only against the exact pre-mutation
    /// document that produced them; anything else is a programmer error.
    pub fn value_at(&self, offset: usize) -> ValueRef<'a> {
        debug_assert!(offset < self.body.len(), "offsets come from this document's own plan");
        read_value(self.body, self.dict, offset).0
    }
}

/// A document a store may hold (ADR-0169 D4): plain canonical v1 bytes,
/// header and body, never interned, nesting at most [`DEPTH_MAX`], with a
/// body of at most [`DOC_BYTES_MAX`](crate::limits::DOC_BYTES_MAX). Every
/// store sink takes one. Its one field is private, so outside this crate a
/// receipt comes only from [`CanonicalDoc::validate`] (a check), from the
/// parser (a check under its limits), or from `apply` and
/// `merge_absent_document`, which mint it from a canonical pre-image and
/// canonical operands — trusted inputs, held by review, the agreement
/// property and their debug postconditions.
///
/// ```compile_fail,E0451
/// let _ = inf_doc::CanonicalDoc { bytes: std::borrow::Cow::Borrowed(&[0u8][..]) };
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CanonicalDoc<'a> {
    bytes: Cow<'a, [u8]>,
}

impl<'a> CanonicalDoc<'a> {
    /// The trust boundary for bytes from anywhere but this crate's writers:
    /// a `DocFull`, a checkpoint image, a COPY source. The
    /// [`TapeDoc::from_bytes`] walk, plus a refusal of the interned form —
    /// stores hold plain bytes and intern only inside the sink (ADR-0038
    /// D3).
    pub fn validate(bytes: &'a [u8]) -> Result<CanonicalDoc<'a>, DocError> {
        TapeDoc::from_bytes(bytes)?;
        if bytes[3] & FLAG_INTERNED != 0 {
            return Err(DocError::NonCanonical("interned form"));
        }
        Ok(CanonicalDoc { bytes: Cow::Borrowed(bytes) })
    }

    /// The parser's accepted output: its limits are the check.
    pub(crate) fn parsed(bytes: &'a [u8]) -> CanonicalDoc<'a> {
        debug_assert!(
            CanonicalDoc::validate(bytes).is_ok(),
            "the parser emits canonical documents"
        );
        CanonicalDoc { bytes: Cow::Borrowed(bytes) }
    }

    /// Header and body.
    #[inline]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// The root value's bytes: the header stripped.
    #[inline]
    pub fn body(&self) -> &[u8] {
        &self.bytes[HEADER_LEN..]
    }

    /// The document as a validated tape.
    #[inline]
    pub fn tape(&self) -> TapeDoc<'_> {
        TapeDoc::from_validated_bytes(&self.bytes)
    }
}

impl CanonicalDoc<'static> {
    /// A document this crate's mutation engine emitted from a canonical
    /// pre-image and canonical operands (ADR-0169 D4's trusted inputs).
    pub(crate) fn emitted(bytes: Vec<u8>) -> CanonicalDoc<'static> {
        CanonicalDoc { bytes: Cow::Owned(bytes) }
    }
}

/// Containers on the deepest path of one validated value — 0 for a
/// scalar — saturating at `DEPTH_MAX + 1`: the composed-depth check's
/// operand walk (ADR-0169 D3). Iterative over a fixed stack of container
/// ends, so it allocates nothing; O(value bytes).
#[allow(
    clippy::arithmetic_side_effects,
    reason = "bound: `open` indexes a DEPTH_MAX array — checked below it before each push and \
              above 0 before each pop; `off + 4` is under a container tag indexed at \
              `off < value.len() <= isize::MAX`; `DEPTH_MAX + 1` is a const"
)]
pub(crate) fn nesting_depth(value: &[u8]) -> usize {
    let mut ends = [0usize; DEPTH_MAX];
    let mut open = 0usize;
    let mut deepest = 0usize;
    let mut off = 0usize;
    while off < value.len() {
        // A child's extent never crosses its parent's: scopes close exactly here.
        while open > 0 && ends[open - 1] == off {
            open -= 1;
        }
        if !matches!(value[off], TAG_OBJ | TAG_ARR) {
            off = skip_value(value, off);
            continue;
        }
        if open == DEPTH_MAX {
            return DEPTH_MAX + 1;
        }
        ends[open] = skip_value(value, off);
        open += 1;
        deepest = deepest.max(open);
        off += 4;
    }
    deepest
}

/// One open container scope during validation.
struct Scope {
    /// Absolute body offset where this container's region ends.
    end: usize,
    /// Objects track key/value alternation; arrays don't.
    is_obj: bool,
    /// In an object: the next element must be a key.
    expects_key: bool,
}

/// Canonicality bookkeeping for interned tapes (ADR-0038 D3's validator
/// scope): every table entry referenced ≥ 2 times, ids in bounds, tabled
/// keys never plain-encoded. Inert (zero work) when `dict` is empty.
struct DictCheck<'a> {
    dict: &'a [u8],
    /// Table key slices sorted for the plain-key exclusion probe.
    sorted: Vec<&'a [u8]>,
    used: Vec<u32>,
}

impl<'a> DictCheck<'a> {
    fn new(dict: &'a [u8]) -> DictCheck<'a> {
        #[cfg(not(feature = "doc-intern-keys"))]
        {
            debug_assert!(dict.is_empty(), "non-empty dicts cannot exist without the feature");
            DictCheck { dict, used: Vec::new(), sorted: Vec::new() }
        }
        #[cfg(feature = "doc-intern-keys")]
        {
            let count =
                if dict.len() >= 2 { u16::from_le_bytes([dict[0], dict[1]]) as usize } else { 0 };
            let mut sorted = Vec::with_capacity(count);
            for id in 0..count {
                sorted.push(
                    crate::intern::dict_region_get(dict, id)
                        .expect("split_dict validated the table"),
                );
            }
            sorted.sort_unstable();
            DictCheck { dict, used: vec![0; count], sorted }
        }
    }

    #[inline]
    fn active(&self) -> bool {
        !self.dict.is_empty()
    }

    fn use_ref(&mut self, id: usize) -> Result<(), DocError> {
        let Some(slot) = self.used.get_mut(id) else {
            return Err(DocError::BadKey); // id out of table bounds
        };
        // Read only as `< 2` (`finish`), so saturation cannot change the verdict.
        *slot = slot.saturating_add(1);
        Ok(())
    }

    fn check_plain_key(&self, key: &[u8]) -> Result<(), DocError> {
        if self.active() && self.sorted.binary_search(&key).is_ok() {
            return Err(DocError::NonCanonical("plain encoding of an interned key"));
        }
        Ok(())
    }

    fn finish(self) -> Result<(), DocError> {
        if self.used.iter().any(|&n| n < 2) {
            return Err(DocError::NonCanonical("intern entry referenced fewer than twice"));
        }
        Ok(())
    }
}

/// Walk a body as exactly one canonical root value. Iterative, explicit
/// stack, depth-capped (the L9 decoder rule). O(n) single pass (plus the
/// dict bookkeeping on interned tapes).
pub(crate) fn validate_body(body: &[u8], dict: &[u8]) -> Result<(), DocError> {
    let mut stack: Vec<Scope> = Vec::new();
    let mut dict_check = DictCheck::new(dict);
    let mut off = 0usize;
    let mut root_seen = false;
    loop {
        // Close every scope that ends exactly here; a scope can only close
        // on its recorded end because child extents never cross it.
        while let Some(top) = stack.last() {
            debug_assert!(off <= top.end);
            if off != top.end {
                break;
            }
            if top.is_obj && !top.expects_key {
                return Err(DocError::BadKey); // closed mid-pair
            }
            stack.pop();
        }
        if off == body.len() {
            break;
        }
        if stack.is_empty() {
            if root_seen {
                return Err(DocError::BadLength); // trailing bytes after root
            }
            root_seen = true;
        }
        let limit = stack.last().map_or(body.len(), |s| s.end);
        let key_position = stack.last().is_some_and(|s| s.is_obj && s.expects_key);
        off = validate_one(body, off, limit, key_position, &mut stack, &mut dict_check)?;
    }
    if !root_seen {
        return Err(DocError::Truncated); // empty body: a document has a root
    }
    debug_assert!(stack.is_empty());
    dict_check.finish()
}

/// Validate one plain canonical headerless idoc value and return its root.
/// This is the trust boundary for `DocDelta` operands (ADR-0043 D4).
pub fn canonical_fragment(bytes: &[u8]) -> Result<ValueRef<'_>, DocError> {
    validate_body(bytes, &[])?;
    let (value, end) = read_value(bytes, Dict::empty(), 0);
    debug_assert_eq!(end, bytes.len());
    Ok(value)
}

/// Validate a single value (or object key) starting at `off`, bounded by
/// `limit`. Pushes a scope for containers; returns the next offset.
#[allow(
    clippy::arithmetic_side_effects,
    reason = "bound: body[off] is indexed first, so off < body.len() <= isize::MAX; every \
              addend is a const <= 9, a u8 or u24 length, or a varint length (<= 10), so each \
              sum fits the 64-bit usize inf-foundation const-asserts"
)]
fn validate_one(
    body: &[u8],
    off: usize,
    limit: usize,
    key_position: bool,
    stack: &mut Vec<Scope>,
    dict_check: &mut DictCheck<'_>,
) -> Result<usize, DocError> {
    debug_assert!(off < limit);
    debug_assert!(limit <= body.len());
    let tag = body[off];
    // Key positions accept only string forms; everything else is a value.
    let is_string = matches!(tag, TAG_STR8 | TAG_STR24) || (FIXSTR_BASE..=0x9F).contains(&tag);
    if key_position {
        if tag == TAG_KEYREF && dict_check.active() {
            if off + 3 > limit {
                return Err(DocError::Truncated);
            }
            let id = u16::from_le_bytes([body[off + 1], body[off + 2]]) as usize;
            dict_check.use_ref(id)?;
            let top = stack.last_mut().expect("key position implies an open object");
            top.expects_key = false;
            return Ok(off + 3);
        }
        if !is_string {
            return Err(DocError::BadKey);
        }
        let (data_at, next) = validate_str(body, off, limit)?;
        dict_check.check_plain_key(&body[data_at..next])?;
        let top = stack.last_mut().expect("key position implies an open object");
        top.expects_key = false;
        return Ok(next);
    }
    // A completed value in an object flips it back to expecting a key.
    // Containers count as the value at push time (matches the builder).
    if let Some(top) = stack.last_mut()
        && top.is_obj
    {
        debug_assert!(!top.expects_key);
        top.expects_key = true;
    }
    if is_string {
        return validate_str(body, off, limit).map(|(_, next)| next);
    }
    if tag.cast_signed() >= FIXINT_MIN_TAG {
        return Ok(off + 1); // fixint: 0x00..=0x7F and 0xE0..=0xFF
    }
    match tag {
        TAG_NULL | TAG_FALSE | TAG_TRUE => Ok(off + 1),
        TAG_I64 => {
            // Truncation and non-minimal encodings are indistinguishable
            // to the varint decoder; both refuse.
            let (raw, used) = varint::decode_u64(&body[off + 1..limit])
                .ok_or(DocError::NonCanonical("i64 varint"))?;
            let v = unzigzag(raw);
            if (FIXINT_MIN..=FIXINT_MAX).contains(&v) {
                return Err(DocError::NonCanonical("fixint-range i64 tag"));
            }
            Ok(off + 1 + used)
        }
        TAG_F64 => {
            let end = off + 9;
            if end > limit {
                return Err(DocError::Truncated);
            }
            let bits = u64::from_le_bytes(body[off + 1..end].try_into().expect("9-byte extent"));
            if !f64::from_bits(bits).is_finite() {
                return Err(DocError::NonCanonical("non-finite f64"));
            }
            Ok(end)
        }
        TAG_OBJ | TAG_ARR => {
            if off + 4 > limit {
                return Err(DocError::Truncated);
            }
            if stack.len() == DEPTH_MAX {
                return Err(DocError::DepthExceeded);
            }
            let len = read_u24(body, off + 1);
            let end = off + 4 + len;
            if end > limit {
                return Err(DocError::BadLength);
            }
            stack.push(Scope { end, is_obj: tag == TAG_OBJ, expects_key: tag == TAG_OBJ });
            Ok(off + 4)
        }
        _ => Err(DocError::BadTag(tag)),
    }
}

/// Validate one string form (canonical width + UTF-8); returns the data
/// offset and the next offset.
#[allow(
    clippy::arithmetic_side_effects,
    reason = "bound: body[off] is indexed first, so off < body.len() <= isize::MAX; every \
              addend is a const <= 9, a u8 or u24 length, or a varint length (<= 10), so each \
              sum fits the 64-bit usize inf-foundation const-asserts; \
              `t - FIXSTR_BASE` is under its `FIXSTR_BASE..=0x9F` guard"
)]
fn validate_str(body: &[u8], off: usize, limit: usize) -> Result<(usize, usize), DocError> {
    let (data_at, len) = match body[off] {
        t if (FIXSTR_BASE..=0x9F).contains(&t) => (off + 1, (t - FIXSTR_BASE) as usize),
        TAG_STR8 => {
            if off + 2 > limit {
                return Err(DocError::Truncated);
            }
            let len = body[off + 1] as usize;
            if len < STR8_MIN_LEN {
                return Err(DocError::NonCanonical("str8 under 32 bytes"));
            }
            (off + 2, len)
        }
        TAG_STR24 => {
            if off + 4 > limit {
                return Err(DocError::Truncated);
            }
            let len = read_u24(body, off + 1);
            if len < STR24_MIN_LEN {
                return Err(DocError::NonCanonical("str24 under 256 bytes"));
            }
            (off + 4, len)
        }
        _ => unreachable!("caller matched a string tag"),
    };
    let end = data_at + len;
    if end > limit {
        return Err(DocError::Truncated);
    }
    if str::from_utf8(&body[data_at..end]).is_err() {
        return Err(DocError::BadUtf8);
    }
    Ok((data_at, end))
}

/// Skip one validated value; O(1) for containers via the u24 length.
#[inline]
#[allow(
    clippy::arithmetic_side_effects,
    reason = "bound: body[off] is indexed first, so off < body.len() <= isize::MAX; every \
              addend is a const <= 9, a u8 or u24 length, or a varint length (<= 10), so each \
              sum fits the 64-bit usize inf-foundation const-asserts; \
              `tag - FIXSTR_BASE` is under its `FIXSTR_BASE..=0x9F` guard"
)]
pub(crate) fn skip_value(body: &[u8], off: usize) -> usize {
    debug_assert!(off < body.len());
    let tag = body[off];
    if tag.cast_signed() >= FIXINT_MIN_TAG {
        return off + 1;
    }
    if (FIXSTR_BASE..=0x9F).contains(&tag) {
        return off + 1 + (tag - FIXSTR_BASE) as usize;
    }
    match tag {
        TAG_NULL | TAG_FALSE | TAG_TRUE => off + 1,
        TAG_I64 => {
            let (_, used) = varint::decode_u64(&body[off + 1..]).expect("validated varint decodes");
            off + 1 + used
        }
        TAG_F64 => off + 9,
        TAG_STR8 => off + 2 + body[off + 1] as usize,
        TAG_STR24 => off + 4 + read_u24(body, off + 1),
        TAG_OBJ | TAG_ARR => off + 4 + read_u24(body, off + 1),
        TAG_KEYREF => off + 3,
        _ => unreachable!("validated tape has no unknown tags"),
    }
}

/// Decode one validated value at `off`; returns it plus the next offset.
#[inline]
#[allow(
    clippy::arithmetic_side_effects,
    reason = "bound: body[off] is indexed first, so off < body.len() <= isize::MAX; every \
              addend is a const <= 9, a u8 or u24 length, or a varint length (<= 10), so each \
              sum fits the 64-bit usize inf-foundation const-asserts; \
              `tag - FIXSTR_BASE` is under its `FIXSTR_BASE..=0x9F` guard"
)]
pub(crate) fn read_value<'a>(body: &'a [u8], dict: Dict<'a>, off: usize) -> (ValueRef<'a>, usize) {
    debug_assert!(off < body.len());
    let tag = body[off];
    if tag.cast_signed() >= FIXINT_MIN_TAG {
        return (ValueRef::I64(i64::from(tag.cast_signed())), off + 1);
    }
    if (FIXSTR_BASE..=0x9F).contains(&tag) {
        let len = (tag - FIXSTR_BASE) as usize;
        return (ValueRef::Str(DocStr(&body[off + 1..off + 1 + len])), off + 1 + len);
    }
    match tag {
        TAG_NULL => (ValueRef::Null, off + 1),
        TAG_FALSE => (ValueRef::Bool(false), off + 1),
        TAG_TRUE => (ValueRef::Bool(true), off + 1),
        TAG_I64 => {
            let (raw, used) =
                varint::decode_u64(&body[off + 1..]).expect("validated varint decodes");
            (ValueRef::I64(unzigzag(raw)), off + 1 + used)
        }
        TAG_F64 => {
            let bits =
                u64::from_le_bytes(body[off + 1..off + 9].try_into().expect("validated extent"));
            (ValueRef::F64(f64::from_bits(bits)), off + 9)
        }
        TAG_STR8 => {
            let len = body[off + 1] as usize;
            (ValueRef::Str(DocStr(&body[off + 2..off + 2 + len])), off + 2 + len)
        }
        TAG_STR24 => {
            let len = read_u24(body, off + 1);
            (ValueRef::Str(DocStr(&body[off + 4..off + 4 + len])), off + 4 + len)
        }
        TAG_OBJ => {
            let len = read_u24(body, off + 1);
            let entries = &body[off + 4..off + 4 + len];
            (ValueRef::Obj(ObjRef { entries, dict }), off + 4 + len)
        }
        TAG_ARR => {
            let len = read_u24(body, off + 1);
            (ValueRef::Arr(ArrRef { body: &body[off + 4..off + 4 + len], dict }), off + 4 + len)
        }
        _ => unreachable!("validated tape has no unknown tags"),
    }
}

/// Read one validated object key: a string form, or an interned-key ref
/// resolved against the dict.
#[allow(
    clippy::arithmetic_side_effects,
    reason = "bound: body[off] is indexed first, so off < body.len() <= isize::MAX and off + 3 \
              fits usize"
)]
fn read_key<'a>(body: &'a [u8], dict: Dict<'a>, off: usize) -> (DocStr<'a>, usize) {
    if body[off] == TAG_KEYREF {
        let id = u16::from_le_bytes([body[off + 1], body[off + 2]]) as usize;
        let bytes = dict.get(id).expect("validated keyref resolves");
        return (DocStr(bytes), off + 3);
    }
    match read_value(body, dict, off) {
        (ValueRef::Str(s), next) => (s, next),
        _ => unreachable!("validated object key positions hold strings"),
    }
}

impl<'a> ObjRef<'a> {
    /// Entry count — walks the entries (no stored count, ADR-0036 D3; the
    /// arena form stores counts natively, and tape-sized documents make
    /// this walk a handful of O(1) skips).
    pub fn len(&self) -> usize {
        self.iter().count()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    #[inline]
    pub fn iter(&self) -> ObjIter<'a> {
        ObjIter { entries: self.entries, dict: self.dict, off: 0 }
    }

    /// First entry whose key equals `key` (memcmp). Canonical writers emit
    /// unique keys; on a non-canonical duplicate-carrying tape this is the
    /// pinned first-match rule (ADR-0036 D5, golden-tested).
    ///
    /// This lookup is the hot half of the depth-N fetch behind the
    /// `JSON.GET ≤ 1.5× GET` gate. The scan is fused (key extent computed
    /// from the tag inline, length compared before bytes — ~20% over
    /// delegating to `read_str`, S02 ledger artifact). Interned tapes
    /// resolve the *needle* to its id once, then compare ids per tabled
    /// entry (ADR-0038 D5); plain tapes pay one predicted branch.
    ///
    /// `#[inline]` (with `skip_value`/`read_value` below) so callers
    /// integrate the scan: out-of-line, every level of a path fetch paid
    /// call/PLT overhead plus a 32-byte `Option<ValueRef>` sret stack
    /// copy — together ~25% of the S02 criterion row (perf annotate).
    /// A/B log (`.artifacts/m3/s02-traverse-opt-20260711/`): this
    /// inlining is the whole win (216 → 164 ns); a word-compare needle
    /// scan (masked 8-byte tag+key test) and a window-fused value skip
    /// were both **Rejected** on top of it (+4.0% and +5.2% respectively
    /// — the inlined loop is instruction-throughput-bound, and the extra
    /// shift/mask work beats the loads it saves; branch-misses ≈ 0).
    #[inline]
    #[allow(
        clippy::arithmetic_side_effects,
        reason = "bound: e[off] is indexed under the loop guard, so off < e.len() <= isize::MAX; \
                  every addend is a const <= 4 or a u8 or u24 length, so each sum fits the \
                  64-bit usize inf-foundation const-asserts; `tag - FIXSTR_BASE` is under its \
                  `FIXSTR_BASE..=0x9F` guard"
    )]
    pub fn get(&self, key: &[u8]) -> Option<ValueRef<'a>> {
        let needle_id = if self.dict.is_empty() { None } else { self.dict.find(key) };
        let e = self.entries;
        let mut off = 0;
        while off < e.len() {
            let tag = e[off];
            if tag == TAG_KEYREF {
                let id = u16::from_le_bytes([e[off + 1], e[off + 2]]);
                let val_at = off + 3;
                if Some(id) == needle_id {
                    return Some(read_value(e, self.dict, val_at).0);
                }
                off = skip_value(e, val_at);
                continue;
            }
            // Keys are string forms; fixstr (≤ 31 bytes) dominates.
            let (key_at, key_len) = if (FIXSTR_BASE..=0x9F).contains(&tag) {
                (off + 1, (tag - FIXSTR_BASE) as usize)
            } else if tag == TAG_STR8 {
                (off + 2, e[off + 1] as usize)
            } else {
                debug_assert_eq!(tag, TAG_STR24, "validated key positions hold strings");
                (off + 4, read_u24(e, off + 1))
            };
            let val_at = key_at + key_len;
            if key_len == key.len() && &e[key_at..val_at] == key {
                return Some(read_value(e, self.dict, val_at).0);
            }
            off = skip_value(e, val_at);
        }
        None
    }
}

#[derive(Clone, Debug)]
pub struct ObjIter<'a> {
    entries: &'a [u8],
    dict: Dict<'a>,
    off: usize,
}

impl<'a> Iterator for ObjIter<'a> {
    type Item = (DocStr<'a>, ValueRef<'a>);

    fn next(&mut self) -> Option<Self::Item> {
        if self.off == self.entries.len() {
            return None;
        }
        let (key, val_at) = read_key(self.entries, self.dict, self.off);
        let (value, next) = read_value(self.entries, self.dict, val_at);
        self.off = next;
        Some((key, value))
    }
}

impl<'a> ArrRef<'a> {
    /// Element count — walks the body (see `ObjRef::len`).
    pub fn len(&self) -> usize {
        self.iter().count()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.body.is_empty()
    }

    #[inline]
    pub fn iter(&self) -> ArrIter<'a> {
        ArrIter { body: self.body, dict: self.dict, off: 0 }
    }

    /// Element at `index` — `index` skips, each O(1). Negative-index
    /// commands resolve against `len()` at the command layer.
    #[allow(
        clippy::arithmetic_side_effects,
        reason = "bound: `remaining == 0` returns on the line above the decrement"
    )]
    pub fn index(&self, index: usize) -> Option<ValueRef<'a>> {
        let mut off = 0;
        let mut remaining = index;
        while off < self.body.len() {
            if remaining == 0 {
                return Some(read_value(self.body, self.dict, off).0);
            }
            off = skip_value(self.body, off);
            remaining -= 1;
        }
        None
    }
}

#[derive(Clone, Debug)]
pub struct ArrIter<'a> {
    body: &'a [u8],
    dict: Dict<'a>,
    off: usize,
}

impl<'a> Iterator for ArrIter<'a> {
    type Item = ValueRef<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.off == self.body.len() {
            return None;
        }
        let (value, next) = read_value(self.body, self.dict, self.off);
        self.off = next;
        Some(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{self, Value};

    #[test]
    fn u24_reader_checks_the_last_full_word() {
        let bytes = [0xEE, 0x56, 0x34, 0x12];
        assert_eq!(read_u24(&bytes, bytes.len() - 3), 0x12_3456);
        assert!(std::panic::catch_unwind(|| read_u24(&bytes, bytes.len() - 2)).is_err());
    }

    #[test]
    fn value_extents_accept_equality_and_refuse_one_byte_short() {
        let mut body = vec![0xEE, TAG_F64];
        body.extend_from_slice(&1.25f64.to_bits().to_le_bytes());
        for (limit, expected) in [(9, Err(DocError::Truncated)), (10, Ok(10))] {
            assert_eq!(
                validate_one(&body, 1, limit, false, &mut Vec::new(), &mut DictCheck::new(&[])),
                expected
            );
        }
        assert_eq!(validate_body(&[TAG_ARR, 1, 0, 0, TAG_NULL], &[]), Ok(()));
        assert_eq!(validate_body(&[TAG_ARR, 2, 0, 0, TAG_NULL], &[]), Err(DocError::BadLength));
    }

    #[test]
    fn zigzag_covers_the_i64_extremes() {
        for (v, u) in [(0, 0), (-1, 1), (1, 2), (-2, 3), (i64::MAX, u64::MAX - 1)] {
            assert_eq!(zigzag(v), u, "zigzag({v})");
            assert_eq!(unzigzag(u), v, "unzigzag({u})");
        }
        assert_eq!(zigzag(i64::MIN), u64::MAX);
        assert_eq!(unzigzag(u64::MAX), i64::MIN);
    }

    /// The fixint tag band is 0x00..=0x7F and 0xE0..=0xFF (-32..=127).
    #[test]
    fn fixint_tag_band_edges() {
        for (tag, v) in [(0x00u8, 0i64), (0x7F, 127), (0xE0, -32), (0xFF, -1)] {
            assert_eq!(validate_body(&[tag], &[]), Ok(()), "tag {tag:#x}");
            let body = [tag];
            let (value, next) = read_value(&body, Dict::empty(), 0);
            assert!(matches!(value, ValueRef::I64(got) if got == v), "tag {tag:#x}");
            assert_eq!(next, 1);
            assert_eq!(skip_value(&[tag], 0), 1);
        }
        assert_eq!(validate_body(&[0xDF], &[]), Err(DocError::BadTag(0xDF)));
    }

    /// String extents at their width floors: exact fit validates and
    /// reads back; one byte short is `Truncated`.
    #[test]
    fn string_extent_edges() {
        for (header, len) in [(vec![TAG_STR8, 32], 32usize), (vec![TAG_STR24, 0, 1, 0], 256)] {
            let mut exact = header.clone();
            exact.extend(core::iter::repeat_n(b'a', len));
            assert_eq!(validate_body(&exact, &[]), Ok(()));
            assert_eq!(skip_value(&exact, 0), exact.len());
            let (value, next) = read_value(&exact, Dict::empty(), 0);
            assert_eq!(next, exact.len());
            let ValueRef::Str(text) = value else { panic!("a string form reads as a string") };
            assert_eq!(text.as_bytes(), &exact[header.len()..]);
            let short = &exact[..exact.len() - 1];
            assert_eq!(validate_body(short, &[]), Err(DocError::Truncated));
        }
        // A header cut inside its length field.
        assert_eq!(validate_body(&[TAG_STR8], &[]), Err(DocError::Truncated));
        assert_eq!(validate_body(&[TAG_STR24, 0, 1], &[]), Err(DocError::Truncated));
        assert_eq!(validate_body(&[TAG_OBJ, 0, 0], &[]), Err(DocError::Truncated));
        // The largest u24 length over an empty region.
        assert_eq!(validate_body(&[TAG_ARR, 0xFF, 0xFF, 0xFF], &[]), Err(DocError::BadLength));
        assert_eq!(validate_body(&[TAG_STR24, 0xFF, 0xFF, 0xFF], &[]), Err(DocError::Truncated));
    }

    #[test]
    fn canonicality_rejections() {
        // 0xA3 carrying a fixint-range value.
        let mut b = vec![TAG_I64];
        inf_foundation::varint::encode_u64(zigzag(5), &mut b);
        assert_eq!(validate_body(&b, &[]), Err(DocError::NonCanonical("fixint-range i64 tag")));
        // str8 under 32 bytes.
        assert_eq!(
            validate_body(&[TAG_STR8, 1, b'x'], &[]),
            Err(DocError::NonCanonical("str8 under 32 bytes"))
        );
        // Non-finite f64.
        let mut nan = vec![TAG_F64];
        nan.extend_from_slice(&f64::NAN.to_bits().to_le_bytes());
        assert_eq!(validate_body(&nan, &[]), Err(DocError::NonCanonical("non-finite f64")));
        // Reserved tag.
        assert_eq!(validate_body(&[0xA9], &[]), Err(DocError::BadTag(0xA9)));
        // Trailing bytes after the root value.
        assert_eq!(validate_body(&[TAG_NULL, TAG_NULL], &[]), Err(DocError::BadLength));
        // Empty body: no root.
        assert_eq!(validate_body(&[], &[]), Err(DocError::Truncated));
    }

    #[test]
    fn container_length_must_cover_exactly() {
        // arr claiming 2 bytes of body but holding one 1-byte value ⇒ the
        // second byte is read as a value and overruns... a truncated child.
        let bad = [TAG_ARR, 2, 0, 0, TAG_NULL, TAG_F64];
        assert!(validate_body(&bad, &[]).is_err());
        // arr whose len overruns the enclosing document.
        assert_eq!(validate_body(&[TAG_ARR, 9, 0, 0, TAG_NULL], &[]), Err(DocError::BadLength));
    }

    #[test]
    fn object_pairing_is_enforced() {
        // Key position must hold a string.
        assert_eq!(validate_body(&[TAG_OBJ, 1, 0, 0, TAG_NULL], &[]), Err(DocError::BadKey));
        // Object closing mid-pair (key without value).
        assert_eq!(validate_body(&[TAG_OBJ, 2, 0, 0, 0x81, b'k'], &[]), Err(DocError::BadKey));
    }

    /// Raw bytes: `depth` arrays around a null. The builder refuses depth
    /// 129 itself (typed — tested in build.rs), so the validator's cap
    /// needs hand-rolled nesting.
    fn raw_nested(depth: usize) -> Vec<u8> {
        let mut body = vec![TAG_NULL];
        for _ in 0..depth {
            let len = body.len();
            let mut outer =
                vec![TAG_ARR, (len & 0xFF) as u8, ((len >> 8) & 0xFF) as u8, (len >> 16) as u8];
            outer.append(&mut body);
            body = outer;
        }
        body
    }

    #[test]
    fn depth_cap_binds_at_129() {
        assert!(validate_body(&raw_nested(DEPTH_MAX), &[]).is_ok());
        assert_eq!(validate_body(&raw_nested(DEPTH_MAX + 1), &[]), Err(DocError::DepthExceeded));
    }

    /// ADR-0169 D3's operand walk: the deepest path, siblings and objects
    /// included, saturating one past the bound.
    #[test]
    fn nesting_depth_counts_the_deepest_container_path() {
        let fragment = |v: &Value| model::encode_fragment(v).expect("fragment");
        assert_eq!(nesting_depth(&fragment(&Value::I64(0))), 0);
        assert_eq!(nesting_depth(&fragment(&Value::Arr(vec![]))), 1);
        let siblings = Value::Arr(vec![
            Value::Arr(vec![Value::I64(1)]),
            Value::Arr(vec![Value::Arr(vec![Value::I64(2)])]),
            Value::I64(3),
        ]);
        assert_eq!(nesting_depth(&fragment(&siblings)), 3);
        let objects = Value::Obj(vec![
            ("a".into(), Value::Obj(vec![("b".into(), Value::Arr(vec![]))])),
            ("c".into(), Value::Str("x".repeat(300))),
        ]);
        assert_eq!(nesting_depth(&fragment(&objects)), 3);
        assert_eq!(nesting_depth(&raw_nested(DEPTH_MAX)), DEPTH_MAX);
        assert_eq!(nesting_depth(&raw_nested(DEPTH_MAX + 1)), DEPTH_MAX + 1);
        assert_eq!(nesting_depth(&raw_nested(DEPTH_MAX + 5)), DEPTH_MAX + 1);
    }

    /// ADR-0169 D4: the receipt's checked constructor accepts what the
    /// parser emits and refuses a 129th container and the interned form.
    #[test]
    fn canonical_doc_validates_what_a_store_may_hold() {
        let parsed = crate::JsonParser::new().parse(br#"{"a":[1,2]}"#).expect("parses");
        let doc = CanonicalDoc::validate(&parsed).expect("parser output is canonical");
        assert_eq!(doc.as_bytes(), &parsed[..]);
        assert_eq!(doc.body(), &parsed[HEADER_LEN..]);
        let mut deep = parsed[..HEADER_LEN].to_vec();
        let body = raw_nested(DEPTH_MAX + 1);
        deep[4..8].copy_from_slice(&u32::try_from(body.len()).expect("small").to_le_bytes());
        deep.extend_from_slice(&body);
        assert_eq!(CanonicalDoc::validate(&deep), Err(DocError::DepthExceeded));
        #[cfg(feature = "doc-intern-keys")]
        {
            let entry = |n: u8| format!(r#"{{"a-repeated-long-key":{n}}}"#);
            let repeated = format!("[{}]", (0..8).map(entry).collect::<Vec<_>>().join(","));
            let plain = crate::JsonParser::new().parse(repeated.as_bytes()).expect("parses");
            let interned = crate::intern::intern(&plain).expect("repeated keys intern");
            assert!(TapeDoc::from_bytes(&interned).is_ok(), "an interned tape validates");
            assert_eq!(
                CanonicalDoc::validate(&interned),
                Err(DocError::NonCanonical("interned form"))
            );
        }
    }

    #[test]
    fn traversal_reads_back_what_was_built() {
        let v = Value::Obj(vec![
            ("name".into(), Value::Str("Lens".into())),
            ("price".into(), Value::I64(4999)),
            ("tags".into(), Value::Arr(vec![Value::Str("optics".into())])),
        ]);
        let bytes = model::encode(&v).expect("encodes");
        let doc = TapeDoc::from_bytes(&bytes).expect("validates");
        let ValueRef::Obj(obj) = doc.root() else { panic!("root is an object") };
        assert_eq!(obj.len(), 3);
        assert!(matches!(obj.get(b"price"), Some(ValueRef::I64(4999))));
        let Some(ValueRef::Arr(tags)) = obj.get(b"tags") else { panic!("tags is an array") };
        assert_eq!(tags.len(), 1);
        assert!(matches!(tags.index(0), Some(ValueRef::Str(s)) if s.as_bytes() == b"optics"));
        assert!(tags.index(1).is_none());
        assert!(obj.get(b"missing").is_none());
    }
}
