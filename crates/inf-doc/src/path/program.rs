//! Path-program bytecode v1 (ADR-0040 D1/D2): the serialized bytes ARE
//! the program — compiled output, S10 cache value, and `DocDelta`
//! payload are one representation. `PathProgram::from_bytes` is the
//! trust boundary (replay revalidates); `read_op` trusts validated
//! bytes (debug asserts only). One program, one encoding (L7): the
//! validator rejects every non-canonical shape.

use std::rc::Rc;

use inf_foundation::varint;

use super::ast::{Member, PathAst, Segment, SliceSpec};
use super::{PROGRAM_BYTES_CEILING, PathError, PathErrorKind, SEGMENTS_MAX, UNION_MEMBERS_MAX};
use crate::tape::{unzigzag, zigzag};

pub(crate) const PROGRAM_VERSION: u8 = 1;
pub(crate) const FLAG_LEGACY: u8 = 0b0000_0001;

pub(crate) const OP_ROOT: u8 = 0x01;
pub(crate) const OP_CHILD: u8 = 0x02;
pub(crate) const OP_CHILD_ANY: u8 = 0x03;
pub(crate) const OP_INDEX: u8 = 0x04;
pub(crate) const OP_SLICE: u8 = 0x05;
pub(crate) const OP_UNION: u8 = 0x06;
pub(crate) const OP_DESCEND: u8 = 0x07;

const SLICE_HAS_START: u8 = 0b001;
const SLICE_HAS_END: u8 = 0b010;
const SLICE_HAS_STEP: u8 = 0b100;

/// A validated path program. Construction happens exactly two ways:
/// `compile` (valid by construction) and `from_bytes` (validating —
/// replay's entry). Equality/hash are byte equality — the S10 cache
/// relies on canonical encoding for that to be semantic equality.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PathProgram {
    // Programs are cell-local cache values. `Rc` makes a cache-hit clone
    // allocation-free without paying an atomic refcount (ADR-0043 D1).
    bytes: Rc<[u8]>,
}

impl PathProgram {
    /// The exact bytes `DocDelta` stores (S17) — no copy, no re-encode.
    #[inline]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    #[inline]
    pub fn is_legacy(&self) -> bool {
        self.bytes[1] & FLAG_LEGACY != 0
    }

    /// `true` for the bare-root program (`$`, legacy `.`/empty): exactly
    /// `version · flags · Root`. Root-shaped commands branch on this —
    /// root `JSON.SET` is a post-image, root `JSON.DEL` is a key delete
    /// (kernel-owned), never a path edit (ADR-0041 D6).
    #[inline]
    pub fn is_root(&self) -> bool {
        self.bytes.len() == 3
    }

    /// Validate foreign bytes (log replay, fuzz) into a program.
    pub fn from_bytes(bytes: &[u8]) -> Result<PathProgram, PathError> {
        validate(bytes)?;
        Ok(PathProgram { bytes: Rc::from(bytes) })
    }

    /// Decode back to an AST (tests, diagnostics, the round-trip
    /// property — never the eval path, which reads ops in place).
    pub fn decode(&self) -> PathAst {
        let legacy = self.is_legacy();
        let mut segments = Vec::new();
        let mut at = 3; // version, flags, Root
        while at < self.bytes.len() {
            let (op, next) = read_op(&self.bytes, at);
            let segment = match op {
                Op::Descend => {
                    let (inner, inner_next) = read_op(&self.bytes, next);
                    at = inner_next;
                    Segment::Descend(Box::new(op_to_segment(inner)))
                }
                other => {
                    at = next;
                    op_to_segment(other)
                }
            };
            segments.push(segment);
        }
        PathAst { legacy, segments }
    }
}

fn op_to_segment(op: Op<'_>) -> Segment {
    match op {
        Op::Child(key) => Segment::Child(key.to_vec()),
        Op::ChildAny => Segment::ChildAny,
        Op::Index(i) => Segment::Index(i),
        Op::Slice(s) => Segment::Slice(s),
        Op::Union(u) => Segment::Union(u.decode_members()),
        Op::Root | Op::Descend => unreachable!("validated position rules"),
    }
}

/// Encode a parsed AST — the only writer, so canonical by construction.
pub(crate) fn encode(ast: &PathAst) -> PathProgram {
    let mut out = Vec::with_capacity(16);
    out.push(PROGRAM_VERSION);
    out.push(if ast.legacy { FLAG_LEGACY } else { 0 });
    out.push(OP_ROOT);
    for segment in &ast.segments {
        encode_segment(&mut out, segment);
    }
    debug_assert!(out.len() < PROGRAM_BYTES_CEILING, "parser caps text < ceiling");
    debug_assert!(validate(&out).is_ok(), "encoder output validates");
    PathProgram { bytes: Rc::from(out) }
}

/// One selector accepted by the allocation-free scalar patch lane
/// (ADR-0043 D1). Every other selector keeps the canonical general
/// evaluator as its single semantic implementation.
#[derive(Copy, Clone, Debug)]
pub(crate) enum SimpleStep<'a> {
    Child(&'a [u8]),
    Index(i64),
}

pub(crate) struct SimpleSteps<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl PathProgram {
    /// True when every selector is inside the M4.5 §3.1 indexable-path
    /// fence: dot/bracket child steps, array index, and the `[*]`
    /// wildcard. Recursive descent, slices, and unions are outside it —
    /// `..` makes per-mutation index maintenance cost proportional to
    /// document size regardless of relevance (ADR-0075 D2.4; growing
    /// the fence is ADR-per-extension). Zero-allocation walk over the
    /// validated bytes, the `simple_steps` shape.
    pub fn within_index_fence(&self) -> bool {
        let mut at = 3; // version, flags, Root
        while at < self.bytes.len() {
            let (op, next) = read_op(&self.bytes, at);
            if !matches!(op, Op::Child(_) | Op::ChildAny | Op::Index(_)) {
                return false;
            }
            at = next;
        }
        true
    }

    /// Zero-allocation step walk over the validated bytes for
    /// structural path comparison (the M4.5-S04 static path-overlap
    /// prune, ADR-0076 D6): the index-fence vocabulary plus [`PathStep::Other`]
    /// for everything outside it. Consumers must treat `Other` as "may
    /// match anything" — the conservative overlap verdict.
    pub fn steps(&self) -> PathSteps<'_> {
        PathSteps { bytes: &self.bytes, at: 3 }
    }

    /// Returns a zero-allocation iterator only when the whole validated
    /// program is a root followed by `Child`/`Index` selectors.
    pub(crate) fn simple_steps(&self) -> Option<SimpleSteps<'_>> {
        let mut at = 3;
        while at < self.bytes.len() {
            let (op, next) = read_op(&self.bytes, at);
            if !matches!(op, Op::Child(_) | Op::Index(_)) {
                return None;
            }
            at = next;
        }
        Some(SimpleSteps { bytes: &self.bytes, at: 3 })
    }
}

impl<'a> Iterator for SimpleSteps<'a> {
    type Item = SimpleStep<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.at == self.bytes.len() {
            return None;
        }
        let (op, next) = read_op(self.bytes, self.at);
        self.at = next;
        Some(match op {
            Op::Child(key) => SimpleStep::Child(key),
            Op::Index(index) => SimpleStep::Index(index),
            _ => unreachable!("simple_steps prevalidated the whole program"),
        })
    }
}

/// One decoded step for structural comparison ([`PathProgram::steps`]).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PathStep<'a> {
    /// A named child selector (`.name` / `['name']`).
    Child(&'a [u8]),
    /// An array index selector (`[i]`, possibly negative).
    Index(i64),
    /// The `[*]` / `.*` wildcard.
    Wild,
    /// Slice, union, or recursive descent — outside the index fence;
    /// consumers must treat it as "may match anything".
    Other,
}

/// Iterator behind [`PathProgram::steps`]. A `Descend` prefix op yields
/// `Other` and leaves its bound selector to the next call — harmless
/// because every consumer stops at the first `Other` (conservative
/// overlap), and exact continuation past one is never consulted.
pub struct PathSteps<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Iterator for PathSteps<'a> {
    type Item = PathStep<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.at >= self.bytes.len() {
            return None;
        }
        let (op, next) = read_op(self.bytes, self.at);
        self.at = next;
        Some(match op {
            Op::Child(key) => PathStep::Child(key),
            Op::Index(index) => PathStep::Index(index),
            Op::ChildAny => PathStep::Wild,
            Op::Slice(_) | Op::Union(_) | Op::Descend => PathStep::Other,
            Op::Root => unreachable!("Root only leads (validated)"),
        })
    }
}

fn encode_segment(out: &mut Vec<u8>, segment: &Segment) {
    match segment {
        Segment::Child(name) => {
            out.push(OP_CHILD);
            varint::encode_u64(name.len() as u64, out);
            out.extend_from_slice(name);
        }
        Segment::ChildAny => out.push(OP_CHILD_ANY),
        Segment::Index(i) => {
            out.push(OP_INDEX);
            varint::encode_u64(zigzag(*i), out);
        }
        Segment::Slice(s) => encode_slice(out, s),
        Segment::Union(members) => {
            out.push(OP_UNION);
            debug_assert!((2..=UNION_MEMBERS_MAX).contains(&members.len()));
            out.push(members.len() as u8);
            for member in members {
                match member {
                    Member::Name(name) => {
                        out.push(OP_CHILD);
                        varint::encode_u64(name.len() as u64, out);
                        out.extend_from_slice(name);
                    }
                    Member::Index(i) => {
                        out.push(OP_INDEX);
                        varint::encode_u64(zigzag(*i), out);
                    }
                    Member::Slice(s) => encode_slice(out, s),
                }
            }
        }
        Segment::Descend(inner) => {
            out.push(OP_DESCEND);
            encode_segment(out, inner); // depth exactly one (AST invariant)
        }
    }
}

fn encode_slice(out: &mut Vec<u8>, s: &SliceSpec) {
    out.push(OP_SLICE);
    let presence = s.start.map_or(0, |_| SLICE_HAS_START)
        | s.end.map_or(0, |_| SLICE_HAS_END)
        | s.step.map_or(0, |_| SLICE_HAS_STEP);
    out.push(presence);
    for field in [s.start, s.end, s.step].into_iter().flatten() {
        varint::encode_u64(zigzag(field), out);
    }
}

/// One decoded op. `Union` defers member decode — eval walks members in
/// place, the AST decode materializes them.
#[derive(Copy, Clone, Debug)]
pub(crate) enum Op<'a> {
    Root,
    Child(&'a [u8]),
    ChildAny,
    Index(i64),
    Slice(SliceSpec),
    Union(UnionRef<'a>),
    Descend,
}

/// A union's member region: `count` members encoded back-to-back.
#[derive(Copy, Clone, Debug)]
pub(crate) struct UnionRef<'a> {
    pub count: u8,
    bytes: &'a [u8],
    members_at: usize,
}

impl<'a> UnionRef<'a> {
    /// Byte offset of member `m` (0-based) — O(m) sequential skips over
    /// L1-hot program bytes; unions cap at 16 members.
    pub(crate) fn member_at(&self, m: u8) -> usize {
        debug_assert!(m < self.count);
        let mut at = self.members_at;
        for _ in 0..m {
            let (_, next) = read_op(self.bytes, at);
            at = next;
        }
        at
    }

    fn decode_members(&self) -> Vec<Member> {
        let mut members = Vec::with_capacity(self.count as usize);
        let mut at = self.members_at;
        for _ in 0..self.count {
            let (op, next) = read_op(self.bytes, at);
            members.push(match op {
                Op::Child(key) => Member::Name(key.to_vec()),
                Op::Index(i) => Member::Index(i),
                Op::Slice(s) => Member::Slice(s),
                _ => unreachable!("validated member kinds"),
            });
            at = next;
        }
        members
    }
}

/// Decode the op at `at` on **validated** bytes; returns it plus the
/// next op's offset (for a union: past the whole member region).
pub(crate) fn read_op(bytes: &[u8], at: usize) -> (Op<'_>, usize) {
    debug_assert!(at < bytes.len(), "validated pc in bounds");
    match bytes[at] {
        OP_ROOT => (Op::Root, at + 1),
        OP_CHILD => {
            let (len, used) = varint::decode_u64(&bytes[at + 1..]).expect("validated varint");
            let data_at = at + 1 + used;
            let next = data_at + len as usize;
            (Op::Child(&bytes[data_at..next]), next)
        }
        OP_CHILD_ANY => (Op::ChildAny, at + 1),
        OP_INDEX => {
            let (raw, used) = varint::decode_u64(&bytes[at + 1..]).expect("validated varint");
            (Op::Index(unzigzag(raw)), at + 1 + used)
        }
        OP_SLICE => {
            let (slice, next) = read_slice(bytes, at);
            (Op::Slice(slice), next)
        }
        OP_UNION => {
            let count = bytes[at + 1];
            let members_at = at + 2;
            let mut next = members_at;
            for _ in 0..count {
                let (_, after) = read_op(bytes, next);
                next = after;
            }
            (Op::Union(UnionRef { count, bytes, members_at }), next)
        }
        OP_DESCEND => (Op::Descend, at + 1),
        _ => unreachable!("validated tape has no unknown opcodes"),
    }
}

fn read_slice(bytes: &[u8], at: usize) -> (SliceSpec, usize) {
    debug_assert_eq!(bytes[at], OP_SLICE);
    let presence = bytes[at + 1];
    let mut spec = SliceSpec::default();
    let mut next = at + 2;
    for (bit, field) in [
        (SLICE_HAS_START, &mut spec.start),
        (SLICE_HAS_END, &mut spec.end),
        (SLICE_HAS_STEP, &mut spec.step),
    ] {
        if presence & bit != 0 {
            let (raw, used) = varint::decode_u64(&bytes[next..]).expect("validated varint");
            *field = Some(unzigzag(raw));
            next += used;
        }
    }
    (spec, next)
}

fn verr<T>(kind: PathErrorKind, offset: usize) -> Result<T, PathError> {
    Err(PathError { offset, kind })
}

/// The validating walk (trust boundary): every rule from ADR-0040 D2.
/// Iterative; bounded by the byte length and `SEGMENTS_MAX`.
fn validate(bytes: &[u8]) -> Result<(), PathError> {
    if bytes.len() < 3 {
        return verr(PathErrorKind::Truncated, bytes.len());
    }
    if bytes.len() >= PROGRAM_BYTES_CEILING {
        return verr(PathErrorKind::PathTooLong, 0);
    }
    if bytes[0] != PROGRAM_VERSION {
        return verr(PathErrorKind::BadVersion, 0);
    }
    if bytes[1] & !FLAG_LEGACY != 0 {
        return verr(PathErrorKind::BadFlags, 1);
    }
    if bytes[2] != OP_ROOT {
        return verr(PathErrorKind::MissingRoot, 2);
    }
    let mut at = 3;
    let mut segments = 0usize;
    let mut descend_pending = false;
    while at < bytes.len() {
        if segments == SEGMENTS_MAX {
            return verr(PathErrorKind::PathTooDeep, at);
        }
        let opcode = bytes[at];
        match opcode {
            OP_DESCEND => {
                if descend_pending {
                    return verr(PathErrorKind::BadOpcode, at); // `Descend Descend`
                }
                descend_pending = true;
                at += 1;
                continue; // the selector it binds is the same segment
            }
            OP_UNION => {
                let Some(&count) = bytes.get(at + 1) else {
                    return verr(PathErrorKind::Truncated, at + 1);
                };
                if !(2..=UNION_MEMBERS_MAX).contains(&(count as usize)) {
                    return verr(PathErrorKind::BadUnionMember, at + 1);
                }
                at += 2;
                for _ in 0..count {
                    at = validate_selector(bytes, at, /*union_member=*/ true)?;
                }
            }
            OP_ROOT => return verr(PathErrorKind::BadOpcode, at), // Root only leads
            _ => at = validate_selector(bytes, at, false)?,
        }
        descend_pending = false;
        segments += 1;
    }
    if descend_pending {
        return verr(PathErrorKind::TrailingDescend, bytes.len());
    }
    if segments == 0 && bytes.len() != 3 {
        return verr(PathErrorKind::Truncated, at);
    }
    Ok(())
}

/// Validate one selector op at `at`; union members restrict the set.
fn validate_selector(bytes: &[u8], at: usize, union_member: bool) -> Result<usize, PathError> {
    let Some(&opcode) = bytes.get(at) else {
        return verr(PathErrorKind::Truncated, at);
    };
    match opcode {
        OP_CHILD => {
            let (len, used) = varint::decode_u64(&bytes[at + 1..])
                .ok_or(PathError { offset: at + 1, kind: PathErrorKind::Truncated })?;
            let data_at = at + 1 + used;
            // `len` is an attacker-controlled varint on this trust
            // boundary; a huge value must reject, never wrap `usize`.
            let Some(next) = (len as usize).checked_add(data_at).filter(|n| *n <= bytes.len())
            else {
                return verr(PathErrorKind::Truncated, at);
            };
            if str::from_utf8(&bytes[data_at..next]).is_err() {
                return verr(PathErrorKind::InvalidUtf8, data_at);
            }
            Ok(next)
        }
        OP_INDEX => {
            let (_, used) = varint::decode_u64(&bytes[at + 1..])
                .ok_or(PathError { offset: at + 1, kind: PathErrorKind::Truncated })?;
            Ok(at + 1 + used)
        }
        OP_SLICE => {
            let Some(&presence) = bytes.get(at + 1) else {
                return verr(PathErrorKind::Truncated, at + 1);
            };
            if presence & !(SLICE_HAS_START | SLICE_HAS_END | SLICE_HAS_STEP) != 0 {
                return verr(PathErrorKind::BadFlags, at + 1);
            }
            let mut next = at + 2;
            for bit in [SLICE_HAS_START, SLICE_HAS_END, SLICE_HAS_STEP] {
                if presence & bit != 0 {
                    let (raw, used) = varint::decode_u64(&bytes[next..])
                        .ok_or(PathError { offset: next, kind: PathErrorKind::Truncated })?;
                    if bit == SLICE_HAS_STEP && unzigzag(raw) == 0 {
                        return verr(PathErrorKind::BadSlice, next);
                    }
                    next += used;
                }
            }
            Ok(next)
        }
        OP_CHILD_ANY if !union_member => Ok(at + 1),
        _ => verr(
            if union_member { PathErrorKind::BadUnionMember } else { PathErrorKind::BadOpcode },
            at,
        ),
    }
}
