//! Access-program form v1 (M4.5-S09, ADR-0080 D2) — the compiled
//! statement: exactly **one** access step (the ADR-0024 D2 planner
//! fence as a type), an optional residual predicate program, and the
//! page spec. The serialized bytes ARE the program (the ADR-0040/0079
//! rule): compiler output, `QueryOp` fabric payload (S11), and EXPLAIN
//! input (S12) are one representation. `from_bytes` is the trust
//! boundary — fabric arrivals revalidate everything, including the
//! embedded residual through `PredicateProgram::from_bytes`.

use std::rc::Rc;

use inf_foundation::varint;
use inf_store::{
    DecodedIndexKey, IndexId, IndexKeyType, MAX_KEY_LEN, NsId, ORDERED_KEY_MAX, index_key_decode,
};

use crate::predicate::PredicateProgram;

/// Format ceiling — the predicate/path class (ADR-0079 D7): programs
/// ride fabric frames and sit behind cursors. The operational bound is
/// the statement-size config (`STATEMENT_BYTES_CEILING`), far smaller.
pub const ACCESS_PROGRAM_BYTES_CEILING: usize = 0xFFFF;

const ACCESS_VERSION: u8 = 1;

// Projection tags. 0x00 permanently invalid (zero-filled corruption).
const PROJECT_DOCUMENTS: u8 = 0x01;
const PROJECT_COUNT: u8 = 0x02;
// Access-step tags; 0x04+ is successor space (ADR-per-extension).
const STEP_PK_GET: u8 = 0x01;
const STEP_INDEX_RANGE: u8 = 0x02;
const STEP_SCAN: u8 = 0x03;
// Range-edge tags.
const EDGE_UNBOUNDED: u8 = 0x00;
const EDGE_INCLUDED: u8 = 0x01;
const EDGE_EXCLUDED: u8 = 0x02;
// Key-type tags (serialized form only — `IndexKeyType` has no stable
// discriminants of its own).
const KEY_TYPE_UTF8: u8 = 0x01;
const KEY_TYPE_I64: u8 = 0x02;
const KEY_TYPE_F64: u8 = 0x03;
const KEY_TYPE_BOOL: u8 = 0x04;

/// What a page returns (ADR-0080 D4). `Count` pages return a partial
/// count and a cursor exactly like `Documents` pages return items —
/// counting a large range is unbounded work in one command otherwise.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Projection {
    Documents,
    Count,
}

/// One end of an index-range probe, in **encoded key bytes** (ADR-0074
/// layouts): the compiler runs the truth-table mapping once; executors
/// compare bytes only. An `Excluded` upper edge may be a byte string no
/// canonical key decodes to (a `begins_with` prefix-successor) — that
/// is by design, and EXPLAIN renders it as hex.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RangeEdge {
    Unbounded,
    Included(Vec<u8>),
    Excluded(Vec<u8>),
}

impl RangeEdge {
    /// True when `key` is inside this edge taken as a **lower** bound.
    pub fn admits_from_below(&self, key: &[u8]) -> bool {
        match self {
            RangeEdge::Unbounded => true,
            RangeEdge::Included(edge) => key >= edge.as_slice(),
            RangeEdge::Excluded(edge) => key > edge.as_slice(),
        }
    }

    /// True when `key` is inside this edge taken as an **upper** bound.
    pub fn admits_from_above(&self, key: &[u8]) -> bool {
        match self {
            RangeEdge::Unbounded => true,
            RangeEdge::Included(edge) => key <= edge.as_slice(),
            RangeEdge::Excluded(edge) => key < edge.as_slice(),
        }
    }

    fn bytes(&self) -> Option<&[u8]> {
        match self {
            RangeEdge::Unbounded => None,
            RangeEdge::Included(b) | RangeEdge::Excluded(b) => Some(b),
        }
    }
}

/// The one access step (ADR-0024 Decision 2): this enum is the
/// grep-able absence of a second candidate path — no field anywhere
/// holds an alternative.
#[derive(Clone, Debug, PartialEq)]
pub enum AccessStep {
    /// Point lookup by primary key (`$key = '…'`). UTF-8 by grammar.
    PkGet { key: Vec<u8> },
    /// One `ready` index, one byte-space interval. The `{index,
    /// generation}` binding makes drop/rebuild-staleness a typed error
    /// at the executing cell (`validate_binding`, ADR-0075 D7);
    /// `key_type` lets the cell pair-assert its registry entry at the
    /// trust boundary. An interval with `lo` above `hi` is valid and
    /// empty (reversed BETWEEN) — never an error, never normalized away
    /// (EXPLAIN renders what was compiled, L7).
    IndexRange {
        index: IndexId,
        generation: u64,
        key_type: IndexKeyType,
        lo: RangeEdge,
        hi: RangeEdge,
    },
    /// Explicit-consent full scan (`FROM ns.SCAN`) — S14 executes.
    Scan,
}

/// The decoded access program — the compiler's build target and
/// `decode`'s output. `encode` is the only writer of program bytes.
#[derive(Clone, Debug, PartialEq)]
pub struct Access {
    pub ns: NsId,
    pub projection: Projection,
    /// Statement `LIMIT`: the total matched cap across pages (SQL
    /// semantics; the per-page bound is the executor's scan budget).
    /// Invariant: `Some(n)` ⇒ `n ≥ 1`, and `Count` carries `None` —
    /// both validated, the second is the D2.2 structural rejection.
    pub limit: Option<u32>,
    pub step: AccessStep,
    pub residual: Option<PredicateProgram>,
}

/// `encode` rejection — compiler-side operating conditions. The S09
/// compiler constructs `Access` values that cannot trip these; they
/// exist because `Access` is a public type (invalid states must fail
/// typed, not silently serialize).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccessBuildError {
    /// PkGet key outside 1..=`MAX_KEY_LEN` bytes.
    BadKey,
    /// Edge bytes empty, over `ORDERED_KEY_MAX`, or mis-sized for a
    /// fixed8 key type.
    BadEdge,
    /// `limit == Some(0)`.
    BadLimit,
    /// `Count` with a limit (ADR-0080 D4 — structurally rejected).
    CountWithLimit,
    ProgramTooLong,
}

/// Decode/validate failure at the trust boundary — `{offset, kind}`,
/// the `PathError`/`PredicateError` shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AccessError {
    pub offset: usize,
    pub kind: AccessErrorKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccessErrorKind {
    Truncated,
    ProgramTooLong,
    BadVersion,
    /// Reserved flag bits set (must-be-zero in v1).
    BadFlags,
    /// Unreadable or non-minimal varint.
    BadVarint,
    /// Unknown projection/step/edge/key-type tag (successor space
    /// rejects — old binaries fail closed).
    BadTag,
    /// A field outside its bound: ns/index id past u32, generation 0,
    /// index id 0, limit past u32, a `Count` program carrying a limit.
    BadField,
    /// PkGet key empty, over `MAX_KEY_LEN`, or not UTF-8.
    BadKey,
    /// Edge bytes empty, over `ORDERED_KEY_MAX`, or mis-sized fixed8.
    BadEdge,
    /// Embedded residual failing `PredicateProgram::from_bytes`.
    BadResidual,
    TrailingBytes,
}

/// A validated access program. Construction happens exactly two ways:
/// `encode` (valid by construction) and `from_bytes` (validating — the
/// fabric/cursor entry). Byte equality is program identity.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct AccessProgram {
    bytes: Rc<[u8]>,
}

impl AccessProgram {
    /// The exact bytes `QueryOp` frames carry (S11).
    #[inline]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Validate foreign bytes (fabric arrival, fuzz) into a program.
    pub fn from_bytes(bytes: &[u8]) -> Result<AccessProgram, AccessError> {
        validate(bytes)?;
        Ok(AccessProgram { bytes: Rc::from(bytes) })
    }

    /// Decode back to the field form (executors, EXPLAIN, the
    /// round-trip law). Cold path — allocates freely.
    pub fn decode(&self) -> Access {
        let bytes: &[u8] = &self.bytes;
        let mut at = 2; // version, flags
        let ns = NsId(read_varint(bytes, &mut at) as u32);
        let projection =
            if bytes[at] == PROJECT_COUNT { Projection::Count } else { Projection::Documents };
        at += 1;
        let limit_raw = read_varint(bytes, &mut at);
        let limit = (limit_raw != 0).then_some(limit_raw as u32);
        let step_tag = bytes[at];
        at += 1;
        let step = match step_tag {
            STEP_PK_GET => AccessStep::PkGet { key: read_bytes(bytes, &mut at) },
            STEP_INDEX_RANGE => decode_index_range(bytes, &mut at),
            _ => AccessStep::Scan,
        };
        let residual_len = read_varint(bytes, &mut at) as usize;
        let residual = (residual_len != 0).then(|| {
            let end = at + residual_len;
            PredicateProgram::from_bytes(&bytes[at..end])
                .expect("validated at the access-program boundary")
        });
        Access { ns, projection, limit, step, residual }
    }

    /// Deterministic EXPLAIN rendering (ADR-0080 D2): stable field
    /// order, typed bounds via the S02 debug decoder with a hex
    /// fallback for boundary byte strings no canonical key produces.
    /// The S09 golden suite pins this text; S12 renders it verbatim.
    pub fn explain(&self) -> String {
        let access = self.decode();
        let mut out = String::with_capacity(256);
        out.push_str(&format!("ns={}\n", access.ns.0));
        let project = match access.projection {
            Projection::Documents => "documents",
            Projection::Count => "count",
        };
        out.push_str(&format!("project={project}\n"));
        if let Some(limit) = access.limit {
            out.push_str(&format!("limit={limit}\n"));
        }
        explain_step(&access.step, &mut out);
        match &access.residual {
            None => out.push_str("residual=none\n"),
            Some(program) => {
                out.push_str("residual:\n");
                crate::predicate::explain_predicate(program, 2, &mut out);
            }
        }
        out
    }
}

fn decode_index_range(bytes: &[u8], at: &mut usize) -> AccessStep {
    let index = IndexId(read_varint(bytes, at) as u32);
    let generation = read_varint(bytes, at);
    let key_type = decode_key_type(bytes[*at]).expect("validated key type");
    *at += 1;
    let lo = decode_edge(bytes, at);
    let hi = decode_edge(bytes, at);
    AccessStep::IndexRange { index, generation, key_type, lo, hi }
}

fn decode_edge(bytes: &[u8], at: &mut usize) -> RangeEdge {
    let tag = bytes[*at];
    *at += 1;
    match tag {
        EDGE_UNBOUNDED => RangeEdge::Unbounded,
        EDGE_INCLUDED => RangeEdge::Included(read_bytes(bytes, at)),
        _ => RangeEdge::Excluded(read_bytes(bytes, at)),
    }
}

// Trusted readers over validated bytes (debug asserts only — the
// `read_op` posture).
fn read_varint(bytes: &[u8], at: &mut usize) -> u64 {
    let (value, used) = varint::decode_u64(&bytes[*at..]).expect("validated varint");
    *at += used;
    value
}

fn read_bytes(bytes: &[u8], at: &mut usize) -> Vec<u8> {
    let len = read_varint(bytes, at) as usize;
    let out = bytes[*at..*at + len].to_vec();
    *at += len;
    out
}

fn decode_key_type(tag: u8) -> Option<IndexKeyType> {
    Some(match tag {
        KEY_TYPE_UTF8 => IndexKeyType::Utf8,
        KEY_TYPE_I64 => IndexKeyType::I64,
        KEY_TYPE_F64 => IndexKeyType::F64,
        KEY_TYPE_BOOL => IndexKeyType::Bool,
        _ => return None,
    })
}

fn key_type_tag(key_type: IndexKeyType) -> u8 {
    match key_type {
        IndexKeyType::Utf8 => KEY_TYPE_UTF8,
        IndexKeyType::I64 => KEY_TYPE_I64,
        IndexKeyType::F64 => KEY_TYPE_F64,
        IndexKeyType::Bool => KEY_TYPE_BOOL,
    }
}

// ---------------------------------------------------------------------
// Encode (the only writer — canonical by construction)
// ---------------------------------------------------------------------

/// Serialize an access program — the compiler's exit into the frozen
/// form (ADR-0080 D2).
pub fn encode(access: &Access) -> Result<AccessProgram, AccessBuildError> {
    match (access.projection, access.limit) {
        (Projection::Count, Some(_)) => return Err(AccessBuildError::CountWithLimit),
        (_, Some(0)) => return Err(AccessBuildError::BadLimit),
        _ => {}
    }
    let mut out = Vec::with_capacity(64);
    out.push(ACCESS_VERSION);
    out.push(0); // flags: must-be-zero in v1
    varint::encode_u64(u64::from(access.ns.0), &mut out);
    out.push(match access.projection {
        Projection::Documents => PROJECT_DOCUMENTS,
        Projection::Count => PROJECT_COUNT,
    });
    varint::encode_u64(u64::from(access.limit.unwrap_or(0)), &mut out);
    encode_step(&access.step, &mut out)?;
    match &access.residual {
        None => varint::encode_u64(0, &mut out),
        Some(program) => {
            varint::encode_u64(program.as_bytes().len() as u64, &mut out);
            out.extend_from_slice(program.as_bytes());
        }
    }
    if out.len() >= ACCESS_PROGRAM_BYTES_CEILING {
        return Err(AccessBuildError::ProgramTooLong);
    }
    debug_assert!(validate(&out).is_ok(), "encoder output validates: {:?}", validate(&out));
    Ok(AccessProgram { bytes: Rc::from(out) })
}

fn encode_step(step: &AccessStep, out: &mut Vec<u8>) -> Result<(), AccessBuildError> {
    match step {
        AccessStep::PkGet { key } => {
            if key.is_empty() || key.len() > MAX_KEY_LEN {
                return Err(AccessBuildError::BadKey);
            }
            debug_assert!(core::str::from_utf8(key).is_ok(), "grammar yields UTF-8 keys");
            out.push(STEP_PK_GET);
            varint::encode_u64(key.len() as u64, out);
            out.extend_from_slice(key);
        }
        AccessStep::IndexRange { index, generation, key_type, lo, hi } => {
            debug_assert!(index.0 >= 1, "id 0 is the reserved null (ADR-0075)");
            debug_assert!(*generation >= 1, "generation 0 is reserved (ADR-0075)");
            out.push(STEP_INDEX_RANGE);
            varint::encode_u64(u64::from(index.0), out);
            varint::encode_u64(*generation, out);
            out.push(key_type_tag(*key_type));
            encode_edge(lo, *key_type, out)?;
            encode_edge(hi, *key_type, out)?;
        }
        AccessStep::Scan => out.push(STEP_SCAN),
    }
    Ok(())
}

fn encode_edge(
    edge: &RangeEdge,
    key_type: IndexKeyType,
    out: &mut Vec<u8>,
) -> Result<(), AccessBuildError> {
    let Some(bytes) = edge.bytes() else {
        out.push(EDGE_UNBOUNDED);
        return Ok(());
    };
    if !edge_len_ok(bytes.len(), key_type) {
        return Err(AccessBuildError::BadEdge);
    }
    out.push(if matches!(edge, RangeEdge::Included(_)) { EDGE_INCLUDED } else { EDGE_EXCLUDED });
    varint::encode_u64(bytes.len() as u64, out);
    out.extend_from_slice(bytes);
    Ok(())
}

/// Fixed8 key types carry exactly 8 edge bytes — a mis-sized bound is
/// corrupt, never a shorter range (ADR-0080 D2.1). Utf8 edges may be
/// prefix images/successors, so anything in 1..=`ORDERED_KEY_MAX` goes.
fn edge_len_ok(len: usize, key_type: IndexKeyType) -> bool {
    if key_type.fixed8() { len == 8 } else { (1..=ORDERED_KEY_MAX).contains(&len) }
}

// ---------------------------------------------------------------------
// Validate (the trust boundary — every rule from ADR-0080 D2)
// ---------------------------------------------------------------------

fn verr<T>(kind: AccessErrorKind, offset: usize) -> Result<T, AccessError> {
    Err(AccessError { offset, kind })
}

fn validate(bytes: &[u8]) -> Result<(), AccessError> {
    if bytes.len() < 2 {
        return verr(AccessErrorKind::Truncated, bytes.len());
    }
    if bytes.len() >= ACCESS_PROGRAM_BYTES_CEILING {
        return verr(AccessErrorKind::ProgramTooLong, 0);
    }
    if bytes[0] != ACCESS_VERSION {
        return verr(AccessErrorKind::BadVersion, 0);
    }
    if bytes[1] != 0 {
        return verr(AccessErrorKind::BadFlags, 1);
    }
    let mut at = 2;
    let ns = validate_varint(bytes, &mut at)?;
    if ns > u64::from(u32::MAX) {
        return verr(AccessErrorKind::BadField, at);
    }
    let projection = validate_tag(bytes, &mut at, &[PROJECT_DOCUMENTS, PROJECT_COUNT])?;
    let limit = validate_varint(bytes, &mut at)?;
    if limit > u64::from(u32::MAX) || (projection == PROJECT_COUNT && limit != 0) {
        return verr(AccessErrorKind::BadField, at);
    }
    validate_step(bytes, &mut at)?;
    validate_residual(bytes, &mut at)?;
    if at != bytes.len() {
        return verr(AccessErrorKind::TrailingBytes, at);
    }
    Ok(())
}

fn validate_step(bytes: &[u8], at: &mut usize) -> Result<(), AccessError> {
    let tag = validate_tag(bytes, at, &[STEP_PK_GET, STEP_INDEX_RANGE, STEP_SCAN])?;
    match tag {
        STEP_PK_GET => {
            let key_at = *at;
            let key = validate_region(bytes, at)?;
            if key.is_empty() || key.len() > MAX_KEY_LEN || core::str::from_utf8(key).is_err() {
                return verr(AccessErrorKind::BadKey, key_at);
            }
        }
        STEP_INDEX_RANGE => {
            let id = validate_varint(bytes, at)?;
            if id == 0 || id > u64::from(u32::MAX) {
                return verr(AccessErrorKind::BadField, *at);
            }
            let generation = validate_varint(bytes, at)?;
            if generation == 0 {
                return verr(AccessErrorKind::BadField, *at);
            }
            let type_at = *at;
            let type_tag = validate_tag(
                bytes,
                at,
                &[KEY_TYPE_UTF8, KEY_TYPE_I64, KEY_TYPE_F64, KEY_TYPE_BOOL],
            )?;
            let key_type = decode_key_type(type_tag).expect("tag just validated");
            let _ = type_at;
            validate_edge(bytes, at, key_type)?;
            validate_edge(bytes, at, key_type)?;
        }
        _ => {} // Scan carries nothing.
    }
    Ok(())
}

fn validate_edge(bytes: &[u8], at: &mut usize, key_type: IndexKeyType) -> Result<(), AccessError> {
    let tag = validate_tag(bytes, at, &[EDGE_UNBOUNDED, EDGE_INCLUDED, EDGE_EXCLUDED])?;
    if tag == EDGE_UNBOUNDED {
        return Ok(());
    }
    let edge_at = *at;
    let edge = validate_region(bytes, at)?;
    if !edge_len_ok(edge.len(), key_type) {
        return verr(AccessErrorKind::BadEdge, edge_at);
    }
    Ok(())
}

fn validate_residual(bytes: &[u8], at: &mut usize) -> Result<(), AccessError> {
    let residual_at = *at;
    let residual = validate_region(bytes, at)?;
    if !residual.is_empty() && PredicateProgram::from_bytes(residual).is_err() {
        return verr(AccessErrorKind::BadResidual, residual_at);
    }
    Ok(())
}

fn validate_varint(bytes: &[u8], at: &mut usize) -> Result<u64, AccessError> {
    let (value, used) = varint::decode_u64(&bytes[*at..])
        .ok_or(AccessError { offset: *at, kind: AccessErrorKind::BadVarint })?;
    *at += used;
    Ok(value)
}

fn validate_tag(bytes: &[u8], at: &mut usize, allowed: &[u8]) -> Result<u8, AccessError> {
    let Some(&tag) = bytes.get(*at) else {
        return verr(AccessErrorKind::Truncated, *at);
    };
    if !allowed.contains(&tag) {
        return verr(AccessErrorKind::BadTag, *at);
    }
    *at += 1;
    Ok(tag)
}

/// Length-prefixed region with the overflow-safe end computation (the
/// ADR-0040 validator rule: attacker lengths reject, never wrap).
fn validate_region<'b>(bytes: &'b [u8], at: &mut usize) -> Result<&'b [u8], AccessError> {
    let len = validate_varint(bytes, at)?;
    let start = *at;
    let end = usize::try_from(len)
        .ok()
        .and_then(|l| l.checked_add(start))
        .filter(|e| *e <= bytes.len())
        .ok_or(AccessError { offset: start, kind: AccessErrorKind::Truncated })?;
    *at = end;
    Ok(&bytes[start..end])
}

// ---------------------------------------------------------------------
// EXPLAIN rendering helpers
// ---------------------------------------------------------------------

fn explain_step(step: &AccessStep, out: &mut String) {
    match step {
        AccessStep::PkGet { key } => {
            let key = core::str::from_utf8(key).expect("validated UTF-8 key");
            out.push_str(&format!("access=pk-get key={key:?}\n"));
        }
        AccessStep::IndexRange { index, generation, key_type, lo, hi } => {
            out.push_str(&format!(
                "access=index-range index={} gen={} type={}\n",
                index.0,
                generation,
                key_type_name(*key_type),
            ));
            out.push_str(&format!("lo={}\n", explain_edge(lo, *key_type)));
            out.push_str(&format!("hi={}\n", explain_edge(hi, *key_type)));
        }
        AccessStep::Scan => out.push_str("access=scan\n"),
    }
}

pub(crate) fn key_type_name(key_type: IndexKeyType) -> &'static str {
    match key_type {
        IndexKeyType::Utf8 => "utf8",
        IndexKeyType::I64 => "i64",
        IndexKeyType::F64 => "f64",
        IndexKeyType::Bool => "bool",
    }
}

fn explain_edge(edge: &RangeEdge, key_type: IndexKeyType) -> String {
    let (word, bytes) = match edge {
        RangeEdge::Unbounded => return "unbounded".to_string(),
        RangeEdge::Included(b) => ("incl", b),
        RangeEdge::Excluded(b) => ("excl", b),
    };
    // Typed rendering where the canonical-strict S02 decoder applies;
    // boundary byte strings (prefix successors, truncated long-literal
    // images) render as hex — honest, deterministic (ADR-0080 D2.4).
    match index_key_decode(key_type, bytes) {
        Ok(DecodedIndexKey::Utf8(s)) => format!("{word} utf8:{s:?}"),
        Ok(DecodedIndexKey::I64(v)) => format!("{word} i64:{v}"),
        Ok(DecodedIndexKey::F64(v)) => format!("{word} f64:{v:?}"),
        Ok(DecodedIndexKey::Bool(v)) => format!("{word} bool:{v}"),
        Err(_) => {
            let mut hex = String::with_capacity(bytes.len() * 2);
            for b in bytes {
                hex.push_str(&format!("{b:02x}"));
            }
            format!("{word} hex:{hex}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::predicate::{CmpOp, Constant, Predicate};

    fn residual() -> PredicateProgram {
        let path = inf_doc::path::compile(b"$.a").expect("path");
        crate::predicate::encode(&Predicate::Cmp {
            op: CmpOp::Gt,
            path,
            constant: Constant::I64(3),
        })
        .expect("encodes")
    }

    fn sample() -> Access {
        Access {
            ns: NsId(7),
            projection: Projection::Documents,
            limit: Some(10),
            step: AccessStep::IndexRange {
                index: IndexId(3),
                generation: 2,
                key_type: IndexKeyType::I64,
                lo: RangeEdge::Included(11i64.wrapping_add(i64::MIN).to_be_bytes().to_vec()),
                hi: RangeEdge::Unbounded,
            },
            residual: Some(residual()),
        }
    }

    /// Round-trip through both construction paths: encode → bytes →
    /// from_bytes → decode is the identity, for every step kind.
    #[test]
    fn round_trips_every_step_kind() {
        let steps = [
            sample().step,
            AccessStep::PkGet { key: b"user:1".to_vec() },
            AccessStep::Scan,
            AccessStep::IndexRange {
                index: IndexId(1),
                generation: 9,
                key_type: IndexKeyType::Utf8,
                lo: RangeEdge::Excluded(vec![0x61]),
                hi: RangeEdge::Excluded(vec![0x62]),
            },
        ];
        for step in steps {
            for residual in [None, Some(residual())] {
                let access = Access {
                    ns: NsId(1),
                    projection: Projection::Documents,
                    limit: None,
                    step: step.clone(),
                    residual,
                };
                let program = encode(&access).expect("encodes");
                let revalidated = AccessProgram::from_bytes(program.as_bytes()).expect("validates");
                assert_eq!(revalidated.decode(), access);
                assert_eq!(revalidated.as_bytes(), program.as_bytes());
            }
        }
    }

    /// Build-side refusals: the public `Access` type cannot silently
    /// serialize invalid states.
    #[test]
    fn encode_refuses_invalid_states() {
        let mut count_with_limit = sample();
        count_with_limit.projection = Projection::Count;
        assert_eq!(encode(&count_with_limit), Err(AccessBuildError::CountWithLimit));
        let mut zero_limit = sample();
        zero_limit.limit = Some(0);
        assert_eq!(encode(&zero_limit), Err(AccessBuildError::BadLimit));
        let mut empty_key = sample();
        empty_key.step = AccessStep::PkGet { key: Vec::new() };
        empty_key.limit = None;
        assert_eq!(encode(&empty_key), Err(AccessBuildError::BadKey));
        let mut long_key = sample();
        long_key.step = AccessStep::PkGet { key: vec![b'k'; MAX_KEY_LEN + 1] };
        long_key.limit = None;
        assert_eq!(encode(&long_key), Err(AccessBuildError::BadKey));
        let mut bad_edge = sample();
        bad_edge.step = AccessStep::IndexRange {
            index: IndexId(1),
            generation: 1,
            key_type: IndexKeyType::I64,
            lo: RangeEdge::Included(vec![0; 7]), // fixed8 wants exactly 8
            hi: RangeEdge::Unbounded,
        };
        assert_eq!(encode(&bad_edge), Err(AccessBuildError::BadEdge));
    }

    /// The trust boundary's negative space: every corruption class is a
    /// typed refusal, never a mis-execution (the fuzz target widens
    /// this; these pin one representative per class).
    #[test]
    fn from_bytes_rejects_each_corruption_class() {
        let valid = encode(&sample()).expect("encodes").as_bytes().to_vec();
        let kind = |bytes: &[u8]| AccessProgram::from_bytes(bytes).expect_err("rejects").kind;
        assert_eq!(kind(&[]), AccessErrorKind::Truncated);
        assert_eq!(kind(&[2, 0, 1]), AccessErrorKind::BadVersion);
        assert_eq!(kind(&[1, 1, 1]), AccessErrorKind::BadFlags);
        let mut bad_projection = valid.clone();
        bad_projection[3] = 0x00; // ns is one varint byte here
        assert_eq!(kind(&bad_projection), AccessErrorKind::BadTag);
        let mut trailing = valid.clone();
        trailing.push(0);
        assert_eq!(kind(&trailing), AccessErrorKind::TrailingBytes);
        let mut truncated = valid.clone();
        truncated.pop();
        assert!(matches!(
            kind(&truncated),
            AccessErrorKind::Truncated | AccessErrorKind::BadResidual
        ));
        // Count + limit has no valid encoding (D2 rule 2): flip the
        // projection byte of a limit-bearing program.
        let mut count_limit = valid.clone();
        count_limit[3] = PROJECT_COUNT;
        assert_eq!(kind(&count_limit), AccessErrorKind::BadField);
        // A residual that is not a valid predicate program.
        let mut no_residual = sample();
        no_residual.residual = None;
        let mut bytes = encode(&no_residual).expect("encodes").as_bytes().to_vec();
        let len = bytes.len();
        bytes[len - 1] = 3; // residual_len 0 → 3, with 3 garbage...
        bytes.extend_from_slice(&[0xFF, 0xFF, 0xFF]);
        assert_eq!(kind(&bytes), AccessErrorKind::BadResidual);
    }

    #[test]
    fn explain_renders_hex_for_non_canonical_edges() {
        let access = Access {
            ns: NsId(1),
            projection: Projection::Documents,
            limit: None,
            step: AccessStep::IndexRange {
                index: IndexId(1),
                generation: 1,
                key_type: IndexKeyType::Utf8,
                // A prefix image: no terminator, so no canonical key
                // decodes to it.
                lo: RangeEdge::Included(vec![0x61, 0x6c]),
                hi: RangeEdge::Excluded(vec![0x61, 0x6d]),
            },
            residual: None,
        };
        let program = encode(&access).expect("encodes");
        let text = program.explain();
        assert!(text.contains("lo=incl hex:616c"), "{text}");
        assert!(text.contains("hi=excl hex:616d"), "{text}");
    }
}
