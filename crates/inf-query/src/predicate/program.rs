//! Predicate-program bytecode v1 (ADR-0079 D2): the serialized bytes
//! ARE the program — compiler output (S09/S13), fabric payload (S11),
//! and EXPLAIN input (S12) are one representation. `from_bytes` is the
//! trust boundary (fabric arrivals revalidate); `read_op` trusts
//! validated bytes (debug asserts only). The canonical rules —
//! first-reference-ordered, duplicate-free, fully-referenced pools;
//! canonical f64 constants; non-legacy embedded paths — are
//! validator-enforced, so accepted bytes re-encode identically from
//! their decoded tree: the fuzz target's standing law.

use std::rc::Rc;

use inf_doc::path::PathProgram;
use inf_foundation::varint;

use super::{
    BOOL_ARITY_MAX, CONSTANTS_MAX, IN_MEMBERS_MAX, NESTING_DEPTH_MAX, OPS_MAX, PATHS_MAX,
    PROGRAM_BYTES_CEILING,
};

pub(crate) const PROGRAM_VERSION: u8 = 1;

// Connectives. 0x00 is permanently invalid (zero-filled corruption is
// detectable, never decodable); 0x04–0x0F is successor connective space.
pub(crate) const OP_AND: u8 = 0x01;
pub(crate) const OP_OR: u8 = 0x02;
pub(crate) const OP_NOT: u8 = 0x03;
// Predicate leaves. 0x1A–0xFF is successor space — ADR-per-opcode-family
// (ADR-0079 D8); unknown opcodes reject, so old binaries fail closed.
pub(crate) const OP_EQ: u8 = 0x10;
pub(crate) const OP_NE: u8 = 0x11;
pub(crate) const OP_LT: u8 = 0x12;
pub(crate) const OP_LE: u8 = 0x13;
pub(crate) const OP_GT: u8 = 0x14;
pub(crate) const OP_GE: u8 = 0x15;
pub(crate) const OP_BETWEEN: u8 = 0x16;
pub(crate) const OP_BEGINS_WITH: u8 = 0x17;
pub(crate) const OP_IN: u8 = 0x18;
pub(crate) const OP_EXISTS: u8 = 0x19;

// Constant-pool tags. 0x00 invalid; 0x05+ successor space.
const CONST_I64: u8 = 0x01;
const CONST_F64: u8 = 0x02;
const CONST_BOOL: u8 = 0x03;
const CONST_UTF8: u8 = 0x04;

/// One comparison operator. `Ne` is existential like every comparator:
/// on a multi-match path it is NOT the negation of `Eq` (ADR-0079 D4 —
/// the subset spec documents the distinction).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl CmpOp {
    fn opcode(self) -> u8 {
        match self {
            CmpOp::Eq => OP_EQ,
            CmpOp::Ne => OP_NE,
            CmpOp::Lt => OP_LT,
            CmpOp::Le => OP_LE,
            CmpOp::Gt => OP_GT,
            CmpOp::Ge => OP_GE,
        }
    }

    fn from_opcode(opcode: u8) -> Option<CmpOp> {
        Some(match opcode {
            OP_EQ => CmpOp::Eq,
            OP_NE => CmpOp::Ne,
            OP_LT => CmpOp::Lt,
            OP_LE => CmpOp::Le,
            OP_GT => CmpOp::Gt,
            OP_GE => CmpOp::Ge,
            _ => return None,
        })
    }
}

/// A typed operand constant (ADR-0079 D2). Constants keep their lexical
/// type — `10` stays i64, `10.0` stays f64 — and the imported ADR-0074
/// compare functions own cross-numeric truth at eval; folding them
/// together at compile would diverge at the 2⁵³/2⁶³ edges.
#[derive(Clone, Debug, PartialEq)]
pub enum Constant {
    I64(i64),
    /// Must be finite; `encode` normalizes `-0.0` to `+0.0` (the VM
    /// compares them equal, so one value gets one encoding — D2.3).
    F64(f64),
    Bool(bool),
    Utf8(String),
}

/// Type family for the D2.4 static checks: BETWEEN bounds share a
/// family and IN members are homogeneous. {I64, F64} mix freely — the
/// ADR-0074 truth table owns cross-numeric semantics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConstFamily {
    Numeric,
    Bool,
    Utf8,
}

impl Constant {
    fn family(&self) -> ConstFamily {
        match self {
            Constant::I64(_) | Constant::F64(_) => ConstFamily::Numeric,
            Constant::Bool(_) => ConstFamily::Bool,
            Constant::Utf8(_) => ConstFamily::Utf8,
        }
    }
}

/// A predicate expression tree — the compilers' (S09/S13) build target
/// and `decode`'s output. `encode` is the only writer of program bytes,
/// so the serialized form is canonical by construction.
#[derive(Clone, Debug, PartialEq)]
pub enum Predicate {
    /// n-ary conjunction, 2..=`BOOL_ARITY_MAX` operands, left-to-right
    /// short-circuit at eval.
    And(Vec<Predicate>),
    /// n-ary disjunction, same bounds and order as `And`.
    Or(Vec<Predicate>),
    /// Flips the verdict only; flags accumulate through it (D5).
    Not(Box<Predicate>),
    /// `path ⊙ constant` under the D4 type table (existential over the
    /// path's match set).
    Cmp { op: CmpOp, path: PathProgram, constant: Constant },
    /// Inclusive on both ends; reversed bounds are valid and
    /// unsatisfiable — always false, never an error (D3).
    Between { path: PathProgram, lo: Constant, hi: Constant },
    /// Prefix is typed `String`, not `Constant`: a non-utf8 operand is
    /// unrepresentable (the D2.4 rule as a type).
    BeginsWith { path: PathProgram, prefix: String },
    /// Membership in a family-homogeneous constant list; each test is
    /// an `Eq` under D4, numeric coercion included.
    In { path: PathProgram, members: Vec<Constant> },
    /// Path presence: ≥ 1 match. Explicit null and containers exist;
    /// never sets the MISSING flag (absence is its domain, D4/D5).
    Exists { path: PathProgram },
}

/// Decode/validate failure at the trust boundary — `{offset, kind}`,
/// the `PathError` shape. Foreign bytes are an operating condition,
/// never a panic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PredicateError {
    pub offset: usize,
    pub kind: PredicateErrorKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PredicateErrorKind {
    /// Bytes end before the structure does.
    Truncated,
    /// Program at or past `PROGRAM_BYTES_CEILING`.
    ProgramTooLong,
    /// Unknown program version.
    BadVersion,
    /// Reserved flag bits set (must-be-zero in v1).
    BadFlags,
    /// Unknown opcode (successor space rejects — fail closed).
    BadOpcode,
    /// Unreadable or non-minimal varint.
    BadVarint,
    /// A count outside its bound (pool sizes, arity, IN members).
    BadCount,
    /// A constant that cannot decode: bad tag, bool byte, non-finite
    /// or `-0.0` f64 pattern, invalid UTF-8.
    BadConstant,
    /// An embedded path failing ADR-0040 validation, or one carrying
    /// the legacy flag (ADR-0079 D1).
    BadPath,
    /// A pool reference at or past its pool's length.
    BadPoolRef,
    /// Canonical-form violation: pool not first-reference ordered,
    /// duplicate entry, or unreferenced entry (ADR-0079 D2.1/D2.2).
    NotCanonical,
    /// BETWEEN bounds or IN members crossing type families, or a
    /// non-utf8 BEGINS_WITH operand (ADR-0079 D2.4).
    BadTypeFamily,
    /// Expression nesting past `NESTING_DEPTH_MAX`.
    TooDeep,
    /// More than `OPS_MAX` ops.
    TooManyOps,
    /// Bytes after the expression's end.
    TrailingBytes,
}

/// `encode` rejection — compiler-side operating conditions (a statement
/// too large or ill-typed for the format), surfaced by S09/S13 as
/// documented statement rejections (L8).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PredicateBuildError {
    TooManyPaths,
    TooManyConstants,
    TooManyOps,
    TooDeep,
    /// AND/OR arity outside 2..=`BOOL_ARITY_MAX`.
    BadArity,
    /// IN members outside 1..=`IN_MEMBERS_MAX`.
    BadInCount,
    /// NaN or ±∞ constant — no surface can produce one (D2.3), so this
    /// is compiler-input hygiene, not a user-reachable path.
    NonFiniteF64,
    MixedBetweenFamilies,
    MixedInFamilies,
    /// Embedded path carries the legacy flag (ADR-0079 D1).
    LegacyPath,
    ProgramTooLong,
}

/// A validated predicate program. Construction happens exactly two
/// ways: `encode` (valid by construction) and `from_bytes` (validating
/// — the fabric/replay entry). Equality/hash are byte equality; `Rc`
/// makes a cache-hit clone allocation-free without an atomic refcount
/// (cell-local, the ADR-0043 D1 argument `PathProgram` also uses).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PredicateProgram {
    bytes: Rc<[u8]>,
}

impl PredicateProgram {
    /// The exact bytes access programs embed and `QueryOp` carries.
    #[inline]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Validate foreign bytes (fabric arrival, fuzz) into a program.
    pub fn from_bytes(bytes: &[u8]) -> Result<PredicateProgram, PredicateError> {
        validate(bytes)?;
        Ok(PredicateProgram { bytes: Rc::from(bytes) })
    }

    /// Decode back to the expression tree (tests, EXPLAIN, the
    /// round-trip law — never the eval path, which reads ops in place).
    pub fn decode(&self) -> Predicate {
        let bytes: &[u8] = &self.bytes;
        let mut at = 2; // version, flags
        let paths = decode_paths(bytes, &mut at);
        let constants = decode_constants(bytes, &mut at);
        decode_expr(bytes, at, &paths, &constants)
    }
}

// ---------------------------------------------------------------------
// Encode (the only writer — canonical by construction)
// ---------------------------------------------------------------------

/// Interning pools, keyed by canonical encoded bytes: every encoding
/// here is bijective (minimal varints, canonical f64 bits, validated
/// path programs), so byte equality is semantic equality — the same
/// argument the validator's duplicate rule stands on.
#[derive(Default)]
struct Pools {
    paths: Vec<Vec<u8>>,
    constants: Vec<Vec<u8>>,
}

impl Pools {
    fn path_id(&mut self, path: &PathProgram) -> Result<u64, PredicateBuildError> {
        if path.is_legacy() {
            return Err(PredicateBuildError::LegacyPath);
        }
        let bytes = path.as_bytes();
        if let Some(i) = self.paths.iter().position(|p| p == bytes) {
            return Ok(i as u64);
        }
        if self.paths.len() == PATHS_MAX {
            return Err(PredicateBuildError::TooManyPaths);
        }
        self.paths.push(bytes.to_vec());
        Ok((self.paths.len() - 1) as u64)
    }

    fn constant_id(&mut self, encoded: Vec<u8>) -> Result<u64, PredicateBuildError> {
        if let Some(i) = self.constants.iter().position(|c| *c == encoded) {
            return Ok(i as u64);
        }
        if self.constants.len() == CONSTANTS_MAX {
            return Err(PredicateBuildError::TooManyConstants);
        }
        self.constants.push(encoded);
        Ok((self.constants.len() - 1) as u64)
    }
}

fn constant_bytes(constant: &Constant) -> Result<Vec<u8>, PredicateBuildError> {
    let mut out = Vec::with_capacity(10);
    match constant {
        Constant::I64(v) => {
            out.push(CONST_I64);
            varint::encode_u64(zigzag(*v), &mut out);
        }
        Constant::F64(f) => {
            if !f.is_finite() {
                return Err(PredicateBuildError::NonFiniteF64);
            }
            // -0.0 normalizes to +0.0: the VM compares them equal, so
            // one value gets one encoding (ADR-0079 D2.3).
            let canonical = if *f == 0.0 { 0.0 } else { *f };
            out.push(CONST_F64);
            out.extend_from_slice(&canonical.to_bits().to_be_bytes());
        }
        Constant::Bool(b) => {
            out.push(CONST_BOOL);
            out.push(u8::from(*b));
        }
        Constant::Utf8(s) => utf8_constant_bytes(s, &mut out),
    }
    Ok(out)
}

fn utf8_constant_bytes(s: &str, out: &mut Vec<u8>) {
    out.push(CONST_UTF8);
    varint::encode_u64(s.len() as u64, out);
    out.extend_from_slice(s.as_bytes());
}

/// Encode an expression tree — the compilers' (S09/S13) exit into the
/// serialized form. Iterative prefix walk: children are pushed reversed
/// so pops run left to right, which is both the evaluation order and
/// the first-reference pool order the validator enforces (D2.1).
pub fn encode(root: &Predicate) -> Result<PredicateProgram, PredicateBuildError> {
    let mut pools = Pools::default();
    let mut expr = Vec::with_capacity(32);
    let mut ops: usize = 0;
    let mut work: Vec<(&Predicate, usize)> = vec![(root, 1)];
    while let Some((node, depth)) = work.pop() {
        // Depth counts every expression level including the leaf — the
        // validator's stack-length rule, stated once in ADR-0079 D7.
        if depth > NESTING_DEPTH_MAX {
            return Err(PredicateBuildError::TooDeep);
        }
        ops += 1;
        if ops > OPS_MAX {
            return Err(PredicateBuildError::TooManyOps);
        }
        match node {
            Predicate::And(children) | Predicate::Or(children) => {
                if !(2..=BOOL_ARITY_MAX).contains(&children.len()) {
                    return Err(PredicateBuildError::BadArity);
                }
                expr.push(if matches!(node, Predicate::And(_)) { OP_AND } else { OP_OR });
                expr.push(children.len() as u8);
                for child in children.iter().rev() {
                    work.push((child, depth + 1));
                }
            }
            Predicate::Not(inner) => {
                expr.push(OP_NOT);
                work.push((inner, depth + 1));
            }
            leaf => encode_leaf(leaf, &mut pools, &mut expr)?,
        }
    }
    assemble(&pools, &expr)
}

fn encode_leaf(
    leaf: &Predicate,
    pools: &mut Pools,
    expr: &mut Vec<u8>,
) -> Result<(), PredicateBuildError> {
    match leaf {
        Predicate::Cmp { op, path, constant } => {
            expr.push(op.opcode());
            emit_ref(pools.path_id(path)?, expr);
            emit_ref(pools.constant_id(constant_bytes(constant)?)?, expr);
        }
        Predicate::Between { path, lo, hi } => {
            if lo.family() != hi.family() {
                return Err(PredicateBuildError::MixedBetweenFamilies);
            }
            expr.push(OP_BETWEEN);
            emit_ref(pools.path_id(path)?, expr);
            emit_ref(pools.constant_id(constant_bytes(lo)?)?, expr);
            emit_ref(pools.constant_id(constant_bytes(hi)?)?, expr);
        }
        Predicate::BeginsWith { path, prefix } => {
            expr.push(OP_BEGINS_WITH);
            emit_ref(pools.path_id(path)?, expr);
            let mut encoded = Vec::with_capacity(prefix.len() + 3);
            utf8_constant_bytes(prefix, &mut encoded);
            emit_ref(pools.constant_id(encoded)?, expr);
        }
        Predicate::In { path, members } => {
            if !(1..=IN_MEMBERS_MAX).contains(&members.len()) {
                return Err(PredicateBuildError::BadInCount);
            }
            if members.iter().any(|m| m.family() != members[0].family()) {
                return Err(PredicateBuildError::MixedInFamilies);
            }
            expr.push(OP_IN);
            emit_ref(pools.path_id(path)?, expr);
            expr.push(members.len() as u8);
            for member in members {
                emit_ref(pools.constant_id(constant_bytes(member)?)?, expr);
            }
        }
        Predicate::Exists { path } => {
            expr.push(OP_EXISTS);
            emit_ref(pools.path_id(path)?, expr);
        }
        Predicate::And(_) | Predicate::Or(_) | Predicate::Not(_) => {
            unreachable!("connectives handled by the walk")
        }
    }
    Ok(())
}

#[inline]
fn emit_ref(id: u64, out: &mut Vec<u8>) {
    varint::encode_u64(id, out);
}

fn assemble(pools: &Pools, expr: &[u8]) -> Result<PredicateProgram, PredicateBuildError> {
    let mut out = Vec::with_capacity(expr.len() + 64);
    out.push(PROGRAM_VERSION);
    out.push(0); // flags: must-be-zero in v1
    varint::encode_u64(pools.paths.len() as u64, &mut out);
    for path in &pools.paths {
        varint::encode_u64(path.len() as u64, &mut out);
        out.extend_from_slice(path);
    }
    varint::encode_u64(pools.constants.len() as u64, &mut out);
    for constant in &pools.constants {
        out.extend_from_slice(constant);
    }
    out.extend_from_slice(expr);
    if out.len() >= PROGRAM_BYTES_CEILING {
        return Err(PredicateBuildError::ProgramTooLong);
    }
    debug_assert!(validate(&out).is_ok(), "encoder output validates: {:?}", validate(&out));
    Ok(PredicateProgram { bytes: Rc::from(out) })
}

/// Zigzag i64 → u64 (the tape/path-program convention: small magnitudes
/// get short varints either sign).
fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

fn unzigzag(u: u64) -> i64 {
    ((u >> 1) as i64) ^ -((u & 1) as i64)
}

// ---------------------------------------------------------------------
// Validate (the trust boundary — every rule from ADR-0079 D2/D7)
// ---------------------------------------------------------------------

fn verr<T>(kind: PredicateErrorKind, offset: usize) -> Result<T, PredicateError> {
    Err(PredicateError { offset, kind })
}

fn validate(bytes: &[u8]) -> Result<(), PredicateError> {
    if bytes.len() < 2 {
        return verr(PredicateErrorKind::Truncated, bytes.len());
    }
    if bytes.len() >= PROGRAM_BYTES_CEILING {
        return verr(PredicateErrorKind::ProgramTooLong, 0);
    }
    if bytes[0] != PROGRAM_VERSION {
        return verr(PredicateErrorKind::BadVersion, 0);
    }
    if bytes[1] != 0 {
        return verr(PredicateErrorKind::BadFlags, 1);
    }
    let mut at = 2;
    let path_spans = validate_paths(bytes, &mut at)?;
    let constants = validate_constants(bytes, &mut at)?;
    validate_expr(bytes, at, path_spans.len(), &constants)
}

fn read_varint_at(bytes: &[u8], at: &mut usize) -> Result<u64, PredicateError> {
    let (value, used) = varint::decode_u64(&bytes[*at..])
        .ok_or(PredicateError { offset: *at, kind: PredicateErrorKind::BadVarint })?;
    *at += used;
    Ok(value)
}

/// Bounds-checked end offset for a length-prefixed region: an
/// attacker-controlled length must reject on overflow or overrun,
/// never wrap (the ADR-0040 validator rule).
fn region_end(len: u64, start: usize, total: usize) -> Option<usize> {
    usize::try_from(len).ok().and_then(|l| l.checked_add(start)).filter(|e| *e <= total)
}

fn validate_paths(bytes: &[u8], at: &mut usize) -> Result<Vec<(usize, usize)>, PredicateError> {
    let count = read_varint_at(bytes, at)?;
    if count > PATHS_MAX as u64 {
        return verr(PredicateErrorKind::BadCount, *at);
    }
    let mut spans: Vec<(usize, usize)> = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let len = read_varint_at(bytes, at)?;
        let start = *at;
        let Some(end) = region_end(len, start, bytes.len()) else {
            return verr(PredicateErrorKind::Truncated, start);
        };
        let Ok(program) = PathProgram::from_bytes(&bytes[start..end]) else {
            return verr(PredicateErrorKind::BadPath, start);
        };
        // Legacy mode is reply-shaping surface; inside a predicate it
        // would only give one path two spellings (ADR-0079 D1).
        if program.is_legacy() {
            return verr(PredicateErrorKind::BadPath, start);
        }
        // Path programs are canonical bytes, so byte equality is
        // semantic equality — a duplicate entry is non-canonical (D2.2).
        if spans.iter().any(|&(s, e)| bytes[s..e] == bytes[start..end]) {
            return verr(PredicateErrorKind::NotCanonical, start);
        }
        spans.push((start, end));
        *at = end;
    }
    Ok(spans)
}

type ConstantSpans = Vec<((usize, usize), ConstFamily)>;

fn validate_constants(bytes: &[u8], at: &mut usize) -> Result<ConstantSpans, PredicateError> {
    let count = read_varint_at(bytes, at)?;
    if count > CONSTANTS_MAX as u64 {
        return verr(PredicateErrorKind::BadCount, *at);
    }
    let mut constants: ConstantSpans = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let start = *at;
        let family = validate_constant(bytes, at)?;
        // Constant encodings are bijective (minimal varints, canonical
        // f64 bits), so byte equality is value equality (D2.2).
        if constants.iter().any(|&((s, e), _)| bytes[s..e] == bytes[start..*at]) {
            return verr(PredicateErrorKind::NotCanonical, start);
        }
        constants.push(((start, *at), family));
    }
    Ok(constants)
}

fn validate_constant(bytes: &[u8], at: &mut usize) -> Result<ConstFamily, PredicateError> {
    let Some(&tag) = bytes.get(*at) else {
        return verr(PredicateErrorKind::Truncated, *at);
    };
    *at += 1;
    match tag {
        CONST_I64 => {
            read_varint_at(bytes, at)?;
            Ok(ConstFamily::Numeric)
        }
        CONST_F64 => {
            let Some(end) = (*at).checked_add(8).filter(|e| *e <= bytes.len()) else {
                return verr(PredicateErrorKind::Truncated, *at);
            };
            let bits = u64::from_be_bytes(bytes[*at..end].try_into().expect("8-byte slice"));
            let f = f64::from_bits(bits);
            // Finite only; the -0.0 pattern is a second spelling of
            // +0.0 and rejects (ADR-0079 D2.3).
            if !f.is_finite() || (f == 0.0 && bits != 0) {
                return verr(PredicateErrorKind::BadConstant, *at);
            }
            *at = end;
            Ok(ConstFamily::Numeric)
        }
        CONST_BOOL => {
            let Some(&b) = bytes.get(*at) else {
                return verr(PredicateErrorKind::Truncated, *at);
            };
            if b > 1 {
                return verr(PredicateErrorKind::BadConstant, *at);
            }
            *at += 1;
            Ok(ConstFamily::Bool)
        }
        CONST_UTF8 => {
            let len = read_varint_at(bytes, at)?;
            let start = *at;
            let Some(end) = region_end(len, start, bytes.len()) else {
                return verr(PredicateErrorKind::Truncated, start);
            };
            if str::from_utf8(&bytes[start..end]).is_err() {
                return verr(PredicateErrorKind::BadConstant, start);
            }
            *at = end;
            Ok(ConstFamily::Utf8)
        }
        _ => verr(PredicateErrorKind::BadConstant, *at - 1),
    }
}

/// Pool-reference state for one expression walk: bounds, first-
/// reference order, and full reference verified in a single pass
/// (D2.1/D2.2 — the k-th distinct reference must be exactly id k).
struct ExprCheck<'a> {
    bytes: &'a [u8],
    constants: &'a [((usize, usize), ConstFamily)],
    path_count: usize,
    next_path: usize,
    next_constant: usize,
}

impl ExprCheck<'_> {
    fn path_ref(&mut self, at: &mut usize) -> Result<(), PredicateError> {
        read_pool_ref(self.bytes, at, self.path_count, &mut self.next_path).map(|_| ())
    }

    fn constant_ref(&mut self, at: &mut usize) -> Result<(ConstFamily, usize), PredicateError> {
        let offset = *at;
        let id = read_pool_ref(self.bytes, at, self.constants.len(), &mut self.next_constant)?;
        Ok((self.constants[id].1, offset))
    }
}

fn read_pool_ref(
    bytes: &[u8],
    at: &mut usize,
    pool_len: usize,
    next_new: &mut usize,
) -> Result<usize, PredicateError> {
    let offset = *at;
    let id = read_varint_at(bytes, at)?;
    if id >= pool_len as u64 {
        return verr(PredicateErrorKind::BadPoolRef, offset);
    }
    let id = id as usize;
    if id > *next_new {
        return verr(PredicateErrorKind::NotCanonical, offset);
    }
    if id == *next_new {
        *next_new += 1;
    }
    Ok(id)
}

fn validate_expr(
    bytes: &[u8],
    mut at: usize,
    path_count: usize,
    constants: &ConstantSpans,
) -> Result<(), PredicateError> {
    let mut check = ExprCheck { bytes, constants, path_count, next_path: 0, next_constant: 0 };
    // Remaining-children frames; the sentinel is the one root
    // expression. An op at nesting depth d is processed with d frames
    // open — the encoder's depth rule, congruent by construction.
    let mut stack: Vec<u8> = Vec::with_capacity(8);
    stack.push(1);
    let mut ops: usize = 0;
    while !stack.is_empty() {
        if stack.len() > NESTING_DEPTH_MAX {
            return verr(PredicateErrorKind::TooDeep, at);
        }
        ops += 1;
        if ops > OPS_MAX {
            return verr(PredicateErrorKind::TooManyOps, at);
        }
        let Some(&opcode) = bytes.get(at) else {
            return verr(PredicateErrorKind::Truncated, at);
        };
        at += 1;
        let leaf = match opcode {
            OP_AND | OP_OR => {
                let Some(&n) = bytes.get(at) else {
                    return verr(PredicateErrorKind::Truncated, at);
                };
                if usize::from(n) < 2 || usize::from(n) > BOOL_ARITY_MAX {
                    return verr(PredicateErrorKind::BadCount, at);
                }
                at += 1;
                stack.push(n);
                false
            }
            OP_NOT => {
                stack.push(1);
                false
            }
            _ => {
                validate_leaf(opcode, &mut at, &mut check)?;
                true
            }
        };
        if leaf {
            // A completed leaf completes every ancestor whose last
            // child it was — cascade until a frame still has children.
            while let Some(top) = stack.last_mut() {
                *top -= 1;
                if *top > 0 {
                    break;
                }
                stack.pop();
            }
        }
    }
    if at != bytes.len() {
        return verr(PredicateErrorKind::TrailingBytes, at);
    }
    // Unreferenced pool entries are dead wire weight — non-canonical.
    if check.next_path != path_count || check.next_constant != constants.len() {
        return verr(PredicateErrorKind::NotCanonical, at);
    }
    Ok(())
}

fn validate_leaf(
    opcode: u8,
    at: &mut usize,
    check: &mut ExprCheck<'_>,
) -> Result<(), PredicateError> {
    match opcode {
        OP_EQ | OP_NE | OP_LT | OP_LE | OP_GT | OP_GE => {
            check.path_ref(at)?;
            check.constant_ref(at)?;
        }
        OP_BETWEEN => {
            check.path_ref(at)?;
            let (lo, _) = check.constant_ref(at)?;
            let (hi, offset) = check.constant_ref(at)?;
            // Bounds share a family or the leaf is statically
            // meaningless under the D4 table (ADR-0079 D2.4).
            if lo != hi {
                return verr(PredicateErrorKind::BadTypeFamily, offset);
            }
        }
        OP_BEGINS_WITH => {
            check.path_ref(at)?;
            let (family, offset) = check.constant_ref(at)?;
            if family != ConstFamily::Utf8 {
                return verr(PredicateErrorKind::BadTypeFamily, offset);
            }
        }
        OP_IN => {
            check.path_ref(at)?;
            let Some(&n) = check.bytes.get(*at) else {
                return verr(PredicateErrorKind::Truncated, *at);
            };
            if usize::from(n) < 1 || usize::from(n) > IN_MEMBERS_MAX {
                return verr(PredicateErrorKind::BadCount, *at);
            }
            *at += 1;
            let (first, _) = check.constant_ref(at)?;
            for _ in 1..n {
                let (family, offset) = check.constant_ref(at)?;
                if family != first {
                    return verr(PredicateErrorKind::BadTypeFamily, offset);
                }
            }
        }
        OP_EXISTS => check.path_ref(at)?,
        _ => return verr(PredicateErrorKind::BadOpcode, *at - 1),
    }
    Ok(())
}

// ---------------------------------------------------------------------
// Trusted reads (validated bytes — the S08 eval path and `decode`)
// ---------------------------------------------------------------------

/// One decoded op on **validated** bytes; `read_op` returns it plus the
/// next op's offset. IN members stay in place ([`InMembersRef`]) — eval
/// iterates them without materializing.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Op<'a> {
    And { arity: u8 },
    Or { arity: u8 },
    Not,
    Cmp { op: CmpOp, path: u32, constant: u32 },
    Between { path: u32, lo: u32, hi: u32 },
    BeginsWith { path: u32, prefix: u32 },
    In { path: u32, members: InMembersRef<'a> },
    Exists { path: u32 },
}

/// An IN op's member region: `count` constant references back-to-back.
#[derive(Clone, Copy, Debug)]
pub(crate) struct InMembersRef<'a> {
    bytes: &'a [u8],
    at: usize,
    count: u8,
}

impl<'a> InMembersRef<'a> {
    pub(crate) fn iter(&self) -> InMembers<'a> {
        InMembers { bytes: self.bytes, at: self.at, left: self.count }
    }
}

pub(crate) struct InMembers<'a> {
    bytes: &'a [u8],
    at: usize,
    left: u8,
}

impl Iterator for InMembers<'_> {
    type Item = u32;

    fn next(&mut self) -> Option<u32> {
        if self.left == 0 {
            return None;
        }
        self.left -= 1;
        Some(trusted_ref(self.bytes, &mut self.at))
    }
}

fn trusted_varint(bytes: &[u8], at: &mut usize) -> u64 {
    let (value, used) = varint::decode_u64(&bytes[*at..]).expect("validated varint");
    *at += used;
    value
}

fn trusted_ref(bytes: &[u8], at: &mut usize) -> u32 {
    let id = trusted_varint(bytes, at);
    debug_assert!(id < CONSTANTS_MAX.max(PATHS_MAX) as u64, "validated pool reference");
    id as u32
}

/// Decode the op at `at` on **validated** bytes; returns it plus the
/// next op's offset (for IN: past the whole member region). ~2–4
/// predictable branches per op — the S08 short-circuit skip walk uses
/// exactly this (skip-by-decode, no stored offsets — ADR-0079 D3).
pub(crate) fn read_op(bytes: &[u8], at: usize) -> (Op<'_>, usize) {
    debug_assert!(at < bytes.len(), "validated pc in bounds");
    let opcode = bytes[at];
    let mut next = at + 1;
    let op = match opcode {
        OP_AND | OP_OR => {
            let arity = bytes[next];
            next += 1;
            if opcode == OP_AND { Op::And { arity } } else { Op::Or { arity } }
        }
        OP_NOT => Op::Not,
        OP_BETWEEN => {
            let path = trusted_ref(bytes, &mut next);
            let lo = trusted_ref(bytes, &mut next);
            let hi = trusted_ref(bytes, &mut next);
            Op::Between { path, lo, hi }
        }
        OP_BEGINS_WITH => {
            let path = trusted_ref(bytes, &mut next);
            let prefix = trusted_ref(bytes, &mut next);
            Op::BeginsWith { path, prefix }
        }
        OP_IN => {
            let path = trusted_ref(bytes, &mut next);
            let count = bytes[next];
            next += 1;
            let members_at = next;
            for _ in 0..count {
                trusted_ref(bytes, &mut next);
            }
            Op::In { path, members: InMembersRef { bytes, at: members_at, count } }
        }
        OP_EXISTS => Op::Exists { path: trusted_ref(bytes, &mut next) },
        _ => {
            let op = CmpOp::from_opcode(opcode).expect("validated tape has no unknown opcodes");
            let path = trusted_ref(bytes, &mut next);
            let constant = trusted_ref(bytes, &mut next);
            Op::Cmp { op, path, constant }
        }
    };
    (op, next)
}

fn decode_paths(bytes: &[u8], at: &mut usize) -> Vec<PathProgram> {
    let count = trusted_varint(bytes, at);
    let mut paths = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let len = trusted_varint(bytes, at) as usize;
        let end = *at + len;
        paths.push(PathProgram::from_bytes(&bytes[*at..end]).expect("validated path program"));
        *at = end;
    }
    paths
}

fn decode_constants(bytes: &[u8], at: &mut usize) -> Vec<Constant> {
    let count = trusted_varint(bytes, at);
    let mut constants = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let tag = bytes[*at];
        *at += 1;
        constants.push(match tag {
            CONST_I64 => Constant::I64(unzigzag(trusted_varint(bytes, at))),
            CONST_F64 => {
                let end = *at + 8;
                let bits = u64::from_be_bytes(bytes[*at..end].try_into().expect("8-byte slice"));
                *at = end;
                Constant::F64(f64::from_bits(bits))
            }
            CONST_BOOL => {
                let b = bytes[*at];
                *at += 1;
                Constant::Bool(b == 1)
            }
            CONST_UTF8 => {
                let len = trusted_varint(bytes, at) as usize;
                let end = *at + len;
                let s = str::from_utf8(&bytes[*at..end]).expect("validated utf8").to_owned();
                *at = end;
                Constant::Utf8(s)
            }
            _ => unreachable!("validated constant tags"),
        });
    }
    constants
}

/// Iterative tree rebuild — a build-frame stack mirrors the prefix
/// walk; no recursion (L9 applies to every decoder, even cold ones).
fn decode_expr(
    bytes: &[u8],
    mut at: usize,
    paths: &[PathProgram],
    constants: &[Constant],
) -> Predicate {
    enum Frame {
        And { arity: u8, children: Vec<Predicate> },
        Or { arity: u8, children: Vec<Predicate> },
        Not,
    }
    let mut frames: Vec<Frame> = Vec::new();
    'ops: loop {
        let (op, next) = read_op(bytes, at);
        at = next;
        let mut completed = match op {
            Op::And { arity } => {
                frames.push(Frame::And { arity, children: Vec::with_capacity(arity.into()) });
                continue 'ops;
            }
            Op::Or { arity } => {
                frames.push(Frame::Or { arity, children: Vec::with_capacity(arity.into()) });
                continue 'ops;
            }
            Op::Not => {
                frames.push(Frame::Not);
                continue 'ops;
            }
            Op::Cmp { op, path, constant } => Predicate::Cmp {
                op,
                path: paths[path as usize].clone(),
                constant: constants[constant as usize].clone(),
            },
            Op::Between { path, lo, hi } => Predicate::Between {
                path: paths[path as usize].clone(),
                lo: constants[lo as usize].clone(),
                hi: constants[hi as usize].clone(),
            },
            Op::BeginsWith { path, prefix } => {
                let Constant::Utf8(prefix) = &constants[prefix as usize] else {
                    unreachable!("validated BEGINS_WITH operand is utf8")
                };
                Predicate::BeginsWith { path: paths[path as usize].clone(), prefix: prefix.clone() }
            }
            Op::In { path, members } => Predicate::In {
                path: paths[path as usize].clone(),
                members: members.iter().map(|id| constants[id as usize].clone()).collect(),
            },
            Op::Exists { path } => Predicate::Exists { path: paths[path as usize].clone() },
        };
        // Fold the completed expression into its ancestors.
        loop {
            match frames.last_mut() {
                None => {
                    debug_assert_eq!(at, bytes.len(), "validated program ends with its expression");
                    return completed;
                }
                Some(Frame::Not) => {
                    frames.pop();
                    completed = Predicate::Not(Box::new(completed));
                }
                Some(Frame::And { arity, children }) => {
                    children.push(completed);
                    if children.len() < usize::from(*arity) {
                        continue 'ops;
                    }
                    let Some(Frame::And { children, .. }) = frames.pop() else {
                        unreachable!("just matched")
                    };
                    completed = Predicate::And(children);
                }
                Some(Frame::Or { arity, children }) => {
                    children.push(completed);
                    if children.len() < usize::from(*arity) {
                        continue 'ops;
                    }
                    let Some(Frame::Or { children, .. }) = frames.pop() else {
                        unreachable!("just matched")
                    };
                    completed = Predicate::Or(children);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use inf_doc::path;
    use proptest::prelude::*;

    use super::*;

    /// Compile a non-legacy path for fixtures; panics only on a broken
    /// fixture, never on tested input.
    fn p(text: &str) -> PathProgram {
        path::compile(text.as_bytes()).expect("fixture path compiles")
    }

    fn exists(text: &str) -> Predicate {
        Predicate::Exists { path: p(text) }
    }

    fn kind_of(bytes: &[u8]) -> PredicateErrorKind {
        PredicateProgram::from_bytes(bytes).expect_err("case must reject").kind
    }

    /// Assemble raw program bytes for validator cases the encoder can
    /// never produce (that unreachability is the point of the tests).
    fn raw(paths: &[&[u8]], constants: &[Vec<u8>], expr: &[u8]) -> Vec<u8> {
        let mut out = vec![PROGRAM_VERSION, 0];
        varint::encode_u64(paths.len() as u64, &mut out);
        for path in paths {
            varint::encode_u64(path.len() as u64, &mut out);
            out.extend_from_slice(path);
        }
        varint::encode_u64(constants.len() as u64, &mut out);
        for constant in constants {
            out.extend_from_slice(constant);
        }
        out.extend_from_slice(expr);
        out
    }

    fn c_i64(v: i64) -> Vec<u8> {
        constant_bytes(&Constant::I64(v)).expect("finite")
    }

    fn c_utf8(s: &str) -> Vec<u8> {
        constant_bytes(&Constant::Utf8(s.to_owned())).expect("utf8")
    }

    fn c_f64_bits(bits: u64) -> Vec<u8> {
        let mut out = vec![0x02];
        out.extend_from_slice(&bits.to_be_bytes());
        out
    }

    // -- Golden vectors (format freeze pins — a change here is a format
    // -- event, not a refactor) ---------------------------------------

    #[test]
    fn golden_exists_program() {
        let program = encode(&exists("$.a")).expect("encodes");
        // header · 1 path ($.a = 6 bytes) · 0 constants · EXISTS path#0.
        let expected = [
            0x01, 0x00, // version, flags
            0x01, 0x06, 0x01, 0x00, 0x01, 0x02, 0x01, 0x61, // path pool
            0x00, // constant pool
            0x19, 0x00, // EXISTS $.a
        ];
        assert_eq!(program.as_bytes(), expected);
    }

    #[test]
    fn golden_and_ge_begins_with() {
        let predicate = Predicate::And(vec![
            Predicate::Cmp { op: CmpOp::Ge, path: p("$.price"), constant: Constant::I64(10) },
            Predicate::BeginsWith { path: p("$.name"), prefix: "ab".to_owned() },
        ]);
        let program = encode(&predicate).expect("encodes");
        let expected = [
            0x01, 0x00, // version, flags
            0x02, // 2 paths
            0x0A, 0x01, 0x00, 0x01, 0x02, 0x05, 0x70, 0x72, 0x69, 0x63, 0x65, // $.price
            0x09, 0x01, 0x00, 0x01, 0x02, 0x04, 0x6E, 0x61, 0x6D, 0x65, // $.name
            0x02, // 2 constants
            0x01, 0x14, // I64(10), zigzag
            0x04, 0x02, 0x61, 0x62, // Utf8("ab")
            0x01, 0x02, // AND, arity 2
            0x15, 0x00, 0x00, // GE path#0 const#0
            0x17, 0x01, 0x01, // BEGINS_WITH path#1 const#1
        ];
        assert_eq!(program.as_bytes(), expected);
        assert_eq!(program.decode(), predicate);
    }

    #[test]
    fn golden_pool_dedup_shares_ids() {
        // The same path and constant referenced twice intern once —
        // first-reference order, one entry each (ADR-0079 D2.1/D2.2).
        let leaf = Predicate::Cmp { op: CmpOp::Eq, path: p("$.a"), constant: Constant::I64(1) };
        let program = encode(&Predicate::Or(vec![leaf.clone(), leaf])).expect("encodes");
        let expected = [
            0x01, 0x00, // version, flags
            0x01, 0x06, 0x01, 0x00, 0x01, 0x02, 0x01, 0x61, // one path
            0x01, 0x01, 0x02, // one constant: I64(1)
            0x02, 0x02, // OR, arity 2
            0x10, 0x00, 0x00, // EQ path#0 const#0
            0x10, 0x00, 0x00, // EQ path#0 const#0 (re-reference)
        ];
        assert_eq!(program.as_bytes(), expected);
    }

    // -- Canonical constants ------------------------------------------

    #[test]
    fn minus_zero_normalizes_to_plus_zero() {
        let predicate =
            Predicate::Cmp { op: CmpOp::Eq, path: p("$.a"), constant: Constant::F64(-0.0) };
        let program = encode(&predicate).expect("encodes");
        let Predicate::Cmp { constant: Constant::F64(decoded), .. } = program.decode() else {
            panic!("shape preserved");
        };
        assert_eq!(decoded.to_bits(), 0, "-0.0 must decode as canonical +0.0");
        // Round-trip equality still holds: -0.0 == +0.0.
        assert_eq!(program.decode(), predicate);
    }

    #[test]
    fn round_trip_covers_every_leaf_shape() {
        let predicate = Predicate::And(vec![
            Predicate::Not(Box::new(exists("$.gone"))),
            Predicate::Between { path: p("$.n"), lo: Constant::I64(3), hi: Constant::F64(9.5) },
            Predicate::In {
                path: p("$.tag"),
                members: vec![Constant::Utf8("a".into()), Constant::Utf8("b".into())],
            },
            Predicate::Cmp { op: CmpOp::Ne, path: p("$.b"), constant: Constant::Bool(true) },
        ]);
        let program = encode(&predicate).expect("encodes");
        let back = PredicateProgram::from_bytes(program.as_bytes()).expect("validates");
        assert_eq!(back.decode(), predicate);
        assert_eq!(encode(&back.decode()).expect("re-encodes").as_bytes(), program.as_bytes());
    }

    // -- Validator rejection matrix (crafted bytes the encoder cannot
    // -- produce) ------------------------------------------------------

    #[test]
    fn rejects_header_damage() {
        let good = encode(&exists("$.a")).expect("encodes").as_bytes().to_vec();
        let mut bad_version = good.clone();
        bad_version[0] = 2;
        assert_eq!(kind_of(&bad_version), PredicateErrorKind::BadVersion);
        let mut bad_flags = good.clone();
        bad_flags[1] = 1;
        assert_eq!(kind_of(&bad_flags), PredicateErrorKind::BadFlags);
        assert_eq!(kind_of(&[]), PredicateErrorKind::Truncated);
        assert_eq!(kind_of(&vec![0u8; PROGRAM_BYTES_CEILING]), PredicateErrorKind::ProgramTooLong);
    }

    #[test]
    fn rejects_every_proper_prefix() {
        // Totality at the trust boundary: no truncation point panics or
        // validates (the expression section is last, so nothing shorter
        // than the whole program can be complete).
        let good = encode(&Predicate::And(vec![
            Predicate::Cmp { op: CmpOp::Ge, path: p("$.price"), constant: Constant::I64(10) },
            Predicate::BeginsWith { path: p("$.name"), prefix: "ab".to_owned() },
        ]))
        .expect("encodes");
        let bytes = good.as_bytes();
        for cut in 0..bytes.len() {
            assert!(PredicateProgram::from_bytes(&bytes[..cut]).is_err(), "prefix {cut}");
        }
    }

    #[test]
    fn rejects_trailing_bytes() {
        let mut bytes = encode(&exists("$.a")).expect("encodes").as_bytes().to_vec();
        bytes.push(OP_EXISTS);
        assert_eq!(kind_of(&bytes), PredicateErrorKind::TrailingBytes);
    }

    #[test]
    fn rejects_unknown_opcodes() {
        let pa = p("$.a");
        for opcode in [0x00u8, 0x04, 0x0F, 0x1A, 0x7F, 0xFF] {
            let bytes = raw(&[pa.as_bytes()], &[], &[opcode, 0x00]);
            assert_eq!(kind_of(&bytes), PredicateErrorKind::BadOpcode, "opcode {opcode:#x}");
        }
    }

    #[test]
    fn rejects_pool_reference_violations() {
        let pa = p("$.a");
        let pb = p("$.b");
        // Out of range.
        let bytes = raw(&[pa.as_bytes()], &[], &[OP_EXISTS, 0x01]);
        assert_eq!(kind_of(&bytes), PredicateErrorKind::BadPoolRef);
        // First reference must be id 0 (first-reference order).
        let bytes = raw(
            &[pa.as_bytes(), pb.as_bytes()],
            &[],
            &[OP_AND, 0x02, OP_EXISTS, 0x01, OP_EXISTS, 0x00],
        );
        assert_eq!(kind_of(&bytes), PredicateErrorKind::NotCanonical);
        // Unreferenced constant.
        let bytes = raw(&[pa.as_bytes()], &[c_i64(1)], &[OP_EXISTS, 0x00]);
        assert_eq!(kind_of(&bytes), PredicateErrorKind::NotCanonical);
        // Duplicate pool entries.
        let bytes = raw(
            &[pa.as_bytes(), pa.as_bytes()],
            &[],
            &[OP_AND, 0x02, OP_EXISTS, 0x00, OP_EXISTS, 0x01],
        );
        assert_eq!(kind_of(&bytes), PredicateErrorKind::NotCanonical);
        let bytes = raw(
            &[pa.as_bytes()],
            &[c_i64(1), c_i64(1)],
            &[OP_AND, 0x02, OP_EQ, 0x00, 0x00, OP_EQ, 0x00, 0x01],
        );
        assert_eq!(kind_of(&bytes), PredicateErrorKind::NotCanonical);
    }

    #[test]
    fn rejects_non_canonical_constants() {
        let pa = p("$.a");
        let expr: &[u8] = &[OP_EQ, 0x00, 0x00];
        for (case, constant) in [
            ("bool byte 2", vec![0x03, 0x02]),
            ("NaN", c_f64_bits(f64::NAN.to_bits())),
            ("-0.0 pattern", c_f64_bits((-0.0f64).to_bits())),
            ("+inf", c_f64_bits(f64::INFINITY.to_bits())),
            ("-inf", c_f64_bits(f64::NEG_INFINITY.to_bits())),
            ("invalid utf8", vec![0x04, 0x01, 0xFF]),
            ("unknown tag", vec![0x05, 0x00]),
        ] {
            let bytes = raw(&[pa.as_bytes()], &[constant], expr);
            assert_eq!(kind_of(&bytes), PredicateErrorKind::BadConstant, "{case}");
        }
    }

    #[test]
    fn rejects_type_family_violations() {
        let pa = p("$.a");
        // BETWEEN bounds across families.
        let bytes = raw(&[pa.as_bytes()], &[c_i64(1), c_utf8("a")], &[OP_BETWEEN, 0, 0, 1]);
        assert_eq!(kind_of(&bytes), PredicateErrorKind::BadTypeFamily);
        // BEGINS_WITH with a non-utf8 operand.
        let bytes = raw(&[pa.as_bytes()], &[c_i64(1)], &[OP_BEGINS_WITH, 0, 0]);
        assert_eq!(kind_of(&bytes), PredicateErrorKind::BadTypeFamily);
        // IN with mixed-family members.
        let bytes = raw(&[pa.as_bytes()], &[c_i64(1), c_utf8("a")], &[OP_IN, 0, 2, 0, 1]);
        assert_eq!(kind_of(&bytes), PredicateErrorKind::BadTypeFamily);
    }

    #[test]
    fn rejects_count_violations() {
        let pa = p("$.a");
        for (case, expr) in [
            ("IN of zero members", vec![OP_IN, 0x00, 0x00]),
            ("IN past the member cap", vec![OP_IN, 0x00, IN_MEMBERS_MAX as u8 + 1]),
            ("arity 1", vec![OP_AND, 0x01, OP_EXISTS, 0x00]),
            ("arity 65", vec![OP_OR, BOOL_ARITY_MAX as u8 + 1]),
        ] {
            let bytes = raw(&[pa.as_bytes()], &[], &expr);
            assert_eq!(kind_of(&bytes), PredicateErrorKind::BadCount, "{case}");
        }
    }

    #[test]
    fn depth_boundary_is_exact() {
        let pa = p("$.a");
        // 31 NOT frames put the leaf at depth 32 — the maximum.
        let mut expr = vec![OP_NOT; NESTING_DEPTH_MAX - 1];
        expr.extend_from_slice(&[OP_EXISTS, 0x00]);
        assert!(PredicateProgram::from_bytes(&raw(&[pa.as_bytes()], &[], &expr)).is_ok());
        // One more NOT pushes the leaf to depth 33.
        let mut expr = vec![OP_NOT; NESTING_DEPTH_MAX];
        expr.extend_from_slice(&[OP_EXISTS, 0x00]);
        assert_eq!(kind_of(&raw(&[pa.as_bytes()], &[], &expr)), PredicateErrorKind::TooDeep);
    }

    #[test]
    fn rejects_too_many_ops() {
        // AND(64) of AND(4 × EXISTS) = 1 + 64 + 256 = 321 ops > 256,
        // at depth 3 — the op bound fires, not the depth bound.
        let pa = p("$.a");
        let mut expr = vec![OP_AND, BOOL_ARITY_MAX as u8];
        for _ in 0..BOOL_ARITY_MAX {
            expr.extend_from_slice(&[OP_AND, 0x04]);
            for _ in 0..4 {
                expr.extend_from_slice(&[OP_EXISTS, 0x00]);
            }
        }
        assert_eq!(kind_of(&raw(&[pa.as_bytes()], &[], &expr)), PredicateErrorKind::TooManyOps);
    }

    #[test]
    fn rejects_non_minimal_varints() {
        // Non-minimal path count [0x80, 0x00]: one value, one encoding.
        let bytes = [PROGRAM_VERSION, 0x00, 0x80, 0x00];
        assert_eq!(kind_of(&bytes), PredicateErrorKind::BadVarint);
    }

    #[test]
    fn rejects_bad_embedded_paths() {
        // Garbage path bytes.
        let bytes = raw(&[&[0xFF, 0xFF]], &[], &[OP_EXISTS, 0x00]);
        assert_eq!(kind_of(&bytes), PredicateErrorKind::BadPath);
        // A legacy-mode program: valid to inf-doc, rejected here (D1).
        let legacy = path::compile(b".a").expect("legacy fixture compiles");
        assert!(legacy.is_legacy(), "fixture must be legacy-mode");
        let bytes = raw(&[legacy.as_bytes()], &[], &[OP_EXISTS, 0x00]);
        assert_eq!(kind_of(&bytes), PredicateErrorKind::BadPath);
    }

    // -- Encoder rejections (compiler-side bounds) ---------------------

    #[test]
    fn encoder_rejects_out_of_bounds_trees() {
        let e = exists("$.a");
        assert_eq!(encode(&Predicate::And(vec![e.clone()])), Err(PredicateBuildError::BadArity));
        assert_eq!(
            encode(&Predicate::And(vec![e.clone(); BOOL_ARITY_MAX + 1])),
            Err(PredicateBuildError::BadArity)
        );
        assert_eq!(
            encode(&Predicate::In { path: p("$.a"), members: vec![] }),
            Err(PredicateBuildError::BadInCount)
        );
        assert_eq!(
            encode(&Predicate::In {
                path: p("$.a"),
                members: vec![Constant::I64(1); IN_MEMBERS_MAX + 1],
            }),
            Err(PredicateBuildError::BadInCount)
        );
        assert_eq!(
            encode(&Predicate::Cmp {
                op: CmpOp::Eq,
                path: p("$.a"),
                constant: Constant::F64(f64::NAN)
            }),
            Err(PredicateBuildError::NonFiniteF64)
        );
        assert_eq!(
            encode(&Predicate::Between {
                path: p("$.a"),
                lo: Constant::I64(1),
                hi: Constant::Utf8("z".into()),
            }),
            Err(PredicateBuildError::MixedBetweenFamilies)
        );
        assert_eq!(
            encode(&Predicate::In {
                path: p("$.a"),
                members: vec![Constant::I64(1), Constant::Bool(true)],
            }),
            Err(PredicateBuildError::MixedInFamilies)
        );
        let legacy = path::compile(b".a").expect("legacy fixture compiles");
        assert_eq!(
            encode(&Predicate::Exists { path: legacy }),
            Err(PredicateBuildError::LegacyPath)
        );
    }

    #[test]
    fn encoder_depth_boundary_matches_validator() {
        let mut tree = exists("$.a");
        for _ in 0..NESTING_DEPTH_MAX - 1 {
            tree = Predicate::Not(Box::new(tree));
        }
        assert!(encode(&tree).is_ok(), "leaf at depth 32 is the maximum");
        assert_eq!(encode(&Predicate::Not(Box::new(tree))), Err(PredicateBuildError::TooDeep));
    }

    #[test]
    fn encoder_rejects_pool_and_size_overflow() {
        // 65 distinct paths.
        let children: Vec<Predicate> =
            (0..BOOL_ARITY_MAX).map(|i| exists(&format!("$.k{i}"))).collect();
        let overflow = Predicate::And(vec![Predicate::And(children), exists("$.k64")]);
        assert_eq!(encode(&overflow), Err(PredicateBuildError::TooManyPaths));
        // 600 distinct constants across six full IN lists.
        let ins: Vec<Predicate> = (0..6)
            .map(|list| Predicate::In {
                path: p("$.a"),
                members: (0..IN_MEMBERS_MAX as i64)
                    .map(|i| Constant::I64(list * 1000 + i))
                    .collect(),
            })
            .collect();
        assert_eq!(encode(&Predicate::And(ins)), Err(PredicateBuildError::TooManyConstants));
        // 321 ops (the validator twin of `rejects_too_many_ops`).
        let inner = Predicate::And(vec![exists("$.a"); 4]);
        let wide = Predicate::And(vec![inner; BOOL_ARITY_MAX]);
        assert_eq!(encode(&wide), Err(PredicateBuildError::TooManyOps));
        // Past the byte ceiling on one giant IN of long strings.
        let big = Predicate::In {
            path: p("$.a"),
            members: (0..IN_MEMBERS_MAX).map(|i| Constant::Utf8(format!("{i:0>700}"))).collect(),
        };
        assert_eq!(encode(&big), Err(PredicateBuildError::ProgramTooLong));
    }

    // -- Generative round-trip + mutation totality ---------------------

    fn arb_path() -> impl Strategy<Value = PathProgram> {
        prop_oneof![
            Just(p("$")),
            Just(p("$.a")),
            Just(p("$.a.b")),
            Just(p("$.items[0]")),
            Just(p("$.items[*].price")),
            Just(p("$..k")),
        ]
    }

    fn arb_string() -> impl Strategy<Value = String> {
        proptest::collection::vec(
            prop_oneof![Just('a'), Just('b'), Just('\0'), Just('é'), Just('9')],
            0..8,
        )
        .prop_map(|chars| chars.into_iter().collect())
    }

    fn arb_f64() -> impl Strategy<Value = f64> {
        prop_oneof![
            any::<f64>().prop_filter("finite constants only", |f| f.is_finite()),
            Just(0.0),
            Just(-0.0),
            Just(f64::MAX),
            Just(5e-324),
        ]
    }

    fn arb_numeric() -> impl Strategy<Value = Constant> {
        prop_oneof![any::<i64>().prop_map(Constant::I64), arb_f64().prop_map(Constant::F64)]
    }

    fn arb_constant() -> impl Strategy<Value = Constant> {
        prop_oneof![
            arb_numeric(),
            any::<bool>().prop_map(Constant::Bool),
            arb_string().prop_map(Constant::Utf8),
        ]
    }

    fn arb_cmp_op() -> impl Strategy<Value = CmpOp> {
        prop_oneof![
            Just(CmpOp::Eq),
            Just(CmpOp::Ne),
            Just(CmpOp::Lt),
            Just(CmpOp::Le),
            Just(CmpOp::Gt),
            Just(CmpOp::Ge),
        ]
    }

    /// Same-family BETWEEN bounds and IN members — the generator
    /// respects D2.4 so `encode` succeeds; the crafted-bytes tests own
    /// the violation side.
    fn arb_leaf() -> impl Strategy<Value = Predicate> {
        prop_oneof![
            (arb_cmp_op(), arb_path(), arb_constant())
                .prop_map(|(op, path, constant)| Predicate::Cmp { op, path, constant }),
            (arb_path(), arb_numeric(), arb_numeric())
                .prop_map(|(path, lo, hi)| Predicate::Between { path, lo, hi }),
            (arb_path(), arb_string(), arb_string()).prop_map(|(path, lo, hi)| {
                Predicate::Between { path, lo: Constant::Utf8(lo), hi: Constant::Utf8(hi) }
            }),
            (arb_path(), arb_string())
                .prop_map(|(path, prefix)| Predicate::BeginsWith { path, prefix }),
            (arb_path(), proptest::collection::vec(arb_numeric(), 1..=5))
                .prop_map(|(path, members)| Predicate::In { path, members }),
            (arb_path(), proptest::collection::vec(arb_string(), 1..=4)).prop_map(
                |(path, members)| Predicate::In {
                    path,
                    members: members.into_iter().map(Constant::Utf8).collect(),
                }
            ),
            arb_path().prop_map(|path| Predicate::Exists { path }),
        ]
    }

    fn arb_predicate() -> impl Strategy<Value = Predicate> {
        arb_leaf().prop_recursive(4, 24, 3, |inner| {
            prop_oneof![
                proptest::collection::vec(inner.clone(), 2..=3).prop_map(Predicate::And),
                proptest::collection::vec(inner.clone(), 2..=3).prop_map(Predicate::Or),
                inner.prop_map(|inner| Predicate::Not(Box::new(inner))),
            ]
        })
    }

    proptest! {
        /// The S07 AC: byte-exact serialization round-trip.
        #[test]
        fn round_trip_is_byte_exact(predicate in arb_predicate()) {
            let program = encode(&predicate).expect("generated predicates are within bounds");
            let back = PredicateProgram::from_bytes(program.as_bytes())
                .expect("encoder output validates");
            let decoded = back.decode();
            prop_assert_eq!(&decoded, &predicate);
            let re = encode(&decoded).expect("decoded tree re-encodes");
            prop_assert_eq!(re.as_bytes(), program.as_bytes());
        }

        /// Totality at the trust boundary (the fuzz target's law, kept
        /// green in the debug lane too).
        #[test]
        fn arbitrary_bytes_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..256)) {
            let _ = PredicateProgram::from_bytes(&bytes);
        }

        /// One flipped byte either rejects typed or yields a program
        /// that is itself canonically stable — never a panic, never a
        /// second encoding of the same meaning.
        #[test]
        fn single_byte_mutations_stay_total_and_canonical(
            predicate in arb_predicate(),
            index in any::<proptest::sample::Index>(),
            byte in any::<u8>(),
        ) {
            let program = encode(&predicate).expect("in bounds");
            let mut bytes = program.as_bytes().to_vec();
            let flip = index.index(bytes.len());
            bytes[flip] = byte;
            if let Ok(mutated) = PredicateProgram::from_bytes(&bytes) {
                let re = encode(&mutated.decode()).expect("accepted bytes re-encode");
                prop_assert_eq!(re.as_bytes(), &bytes[..]);
            }
        }
    }
}
