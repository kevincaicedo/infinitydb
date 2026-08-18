//! Statement compilation (M4.5-S09, ADR-0080 D1/D3): resolve the one
//! access step, construct index range bounds over the S02 encoding,
//! and emit the residual as an ADR-0079 predicate program. Total:
//! every non-compiling statement takes a documented rejection; nothing
//! here ever compares two candidate plans — ambiguity is a refusal
//! (the ADR-0024 D2 fence).

use core::cmp::Ordering;

use inf_doc::path::PathStep;
use inf_store::{
    IndexKeyBuf, IndexKeyType, IndexScalar, IndexSpec, IndexState, ORDERED_KEY_MAX,
    compare_i64_f64, index_key_encode, index_key_escape_prefix, index_scalar_coerce,
};

use super::parse::{Cond, Leaf, LeafKind, Lit, Statement, StmtPath, Target};
use super::{CatalogView, CompiledStatement, QlError, QlErrorKind};
use crate::access::{self, Access, AccessStep, Projection, RangeEdge};
use crate::predicate::{
    self, BOOL_ARITY_MAX, CmpOp, Constant, Predicate, PredicateBuildError, PredicateVm,
};

fn err<T>(offset: usize, kind: QlErrorKind) -> Result<T, QlError> {
    Err(QlError { offset, kind })
}

pub(crate) fn compile_statement<C: CatalogView>(
    statement: Statement,
    catalog: &C,
) -> Result<CompiledStatement, QlError> {
    if statement.projection == Projection::Count && statement.limit.is_some() {
        return err(0, QlErrorKind::CountWithLimit);
    }
    let ns_name = match &statement.target {
        Target::Ns { ns } | Target::Index { ns, .. } | Target::Scan { ns } => ns.clone(),
    };
    let Some(ns) = catalog.resolve_ns(&ns_name) else {
        return err(0, QlErrorKind::UnknownNamespace(name_string(&ns_name)));
    };
    let conjuncts = top_conjuncts(statement.condition);
    let (step, residual_conjuncts) = match &statement.target {
        Target::Scan { .. } => {
            reject_key_pseudo_paths(&conjuncts, true)?;
            (AccessStep::Scan, keep_all(&conjuncts))
        }
        Target::Ns { .. } => resolve_ns_target(&conjuncts, ns, catalog)?,
        Target::Index { index, .. } => {
            reject_key_pseudo_paths(&conjuncts, false)?;
            resolve_named_index(&conjuncts, ns, index, catalog)?
        }
    };
    let residual = build_residual(&conjuncts, &residual_conjuncts, statement.where_at)?;
    let access = Access {
        ns,
        projection: statement.projection,
        limit: statement.limit,
        step,
        residual: residual.clone(),
    };
    let program = access::encode(&access).map_err(|e| {
        debug_assert!(
            matches!(e, access::AccessBuildError::ProgramTooLong),
            "compilation validates every other encode precondition: {e:?}"
        );
        QlError { offset: 0, kind: QlErrorKind::ProgramTooLong }
    })?;
    let vm = residual.as_ref().map(PredicateVm::new);
    Ok(CompiledStatement { program, access, vm })
}

/// The WHERE clause's top-level AND conjuncts — the units resolution
/// examines (spec §5).
fn top_conjuncts(condition: Option<Cond>) -> Vec<Cond> {
    match condition {
        None => Vec::new(),
        Some(Cond::And(v)) => v,
        Some(other) => vec![other],
    }
}

fn keep_all(conjuncts: &[Cond]) -> Vec<usize> {
    (0..conjuncts.len()).collect()
}

// ---------------------------------------------------------------------
// $key rules (spec §4)
// ---------------------------------------------------------------------

/// Rejects `$key` anywhere under a target that cannot serve it: scan
/// targets (`PkWithScan`) and explicitly named indexes (`PkPosition` —
/// `$key` is not a document path, so it cannot ride the residual).
fn reject_key_pseudo_paths(conjuncts: &[Cond], scan_target: bool) -> Result<(), QlError> {
    for conjunct in conjuncts {
        if let Some(at) = find_nested_key(conjunct) {
            let kind = if scan_target { QlErrorKind::PkWithScan } else { QlErrorKind::PkPosition };
            return err(at, kind);
        }
    }
    Ok(())
}

/// Iterative subtree scan for a `$key` leaf (never on the data plane;
/// bounded by the parse-time depth/size caps).
fn find_nested_key(cond: &Cond) -> Option<usize> {
    let mut work: Vec<&Cond> = vec![cond];
    while let Some(node) = work.pop() {
        match node {
            Cond::And(children) | Cond::Or(children) => work.extend(children.iter()),
            Cond::Not(inner) => work.push(inner),
            Cond::Leaf(Leaf { at, kind: LeafKind::KeyEq { .. } }) => return Some(*at),
            Cond::Leaf(_) => {}
        }
    }
    None
}

// ---------------------------------------------------------------------
// Resolution (ADR-0080 D1 — total, planner-free)
// ---------------------------------------------------------------------

/// `FROM ns`: `$key` wins by rule when present; otherwise path
/// matching with ambiguity-as-rejection.
fn resolve_ns_target<C: CatalogView>(
    conjuncts: &[Cond],
    ns: inf_store::NsId,
    catalog: &C,
) -> Result<(AccessStep, Vec<usize>), QlError> {
    let mut key_conjunct: Option<(usize, &Leaf)> = None;
    for (i, conjunct) in conjuncts.iter().enumerate() {
        if let Cond::Leaf(leaf @ Leaf { kind: LeafKind::KeyEq { .. }, .. }) = conjunct {
            if key_conjunct.is_some() {
                return err(leaf.at, QlErrorKind::PkDuplicate);
            }
            key_conjunct = Some((i, leaf));
            continue;
        }
        if let Some(at) = find_nested_key(conjunct) {
            return err(at, QlErrorKind::PkPosition);
        }
    }
    if let Some((i, leaf)) = key_conjunct {
        let LeafKind::KeyEq { key } = &leaf.kind else { unreachable!("matched above") };
        if key.is_empty() || key.len() > inf_store::MAX_KEY_LEN {
            return err(leaf.at, QlErrorKind::PkKeyLength);
        }
        let residual = (0..conjuncts.len()).filter(|&j| j != i).collect();
        return Ok((AccessStep::PkGet { key: key.clone().into_bytes() }, residual));
    }
    let chosen = match_one_ready_index(conjuncts, ns, catalog)?;
    fold_index_bounds(conjuncts, chosen, false)
}

/// Path matching: candidates are `ready` indexes whose declared path
/// equals a servable conjunct's path (equality only on multi-valued
/// paths). Exactly one serves; two is a refusal; zero names the most
/// actionable miss (spec §5).
fn match_one_ready_index<'c, C: CatalogView>(
    conjuncts: &[Cond],
    ns: inf_store::NsId,
    catalog: &'c C,
) -> Result<&'c IndexSpec, QlError> {
    let mut ready: Vec<&IndexSpec> = Vec::new();
    let mut not_ready: Vec<&IndexSpec> = Vec::new();
    for spec in catalog.indexes(ns) {
        let multi_valued = path_is_multi_valued(spec);
        let matched = conjuncts.iter().any(|conjunct| {
            servable(conjunct).is_some_and(|(path, op)| {
                path.program.as_bytes() == spec.program
                    && (!multi_valued || matches!(op, KeyOp::Cmp(CmpOp::Eq, _)))
            })
        });
        if !matched {
            continue;
        }
        if spec.state == IndexState::Ready {
            ready.push(spec);
        } else {
            not_ready.push(spec);
        }
    }
    match ready.len() {
        1 => Ok(ready[0]),
        0 => match not_ready.as_slice() {
            [only] => err(
                0,
                QlErrorKind::IndexNotReady {
                    name: name_string(&only.name),
                    state: only.state.name(),
                },
            ),
            _ => err(0, QlErrorKind::NoAccessPath),
        },
        _ => {
            // Deterministic diagnostic regardless of catalog order.
            let mut names: Vec<String> = ready.iter().map(|s| name_string(&s.name)).collect();
            names.sort();
            err(
                0,
                QlErrorKind::AmbiguousKeyCondition {
                    first: names[0].clone(),
                    second: names[1].clone(),
                },
            )
        }
    }
}

/// `FROM ns.index`: explicit naming — all ambiguity gone; the WHERE
/// must constrain the named index (spec §5).
fn resolve_named_index<C: CatalogView>(
    conjuncts: &[Cond],
    ns: inf_store::NsId,
    index: &[u8],
    catalog: &C,
) -> Result<(AccessStep, Vec<usize>), QlError> {
    let Some(spec) = catalog.index_by_name(ns, index) else {
        return err(0, QlErrorKind::UnknownIndex(name_string(index)));
    };
    if spec.state != IndexState::Ready {
        return err(
            0,
            QlErrorKind::IndexNotReady { name: name_string(&spec.name), state: spec.state.name() },
        );
    }
    fold_index_bounds(conjuncts, spec, true)
}

// ---------------------------------------------------------------------
// Bound folding (ADR-0080 D3)
// ---------------------------------------------------------------------

/// Servable key-condition shapes (spec §5): `=`, `<`, `<=`, `>`, `>=`,
/// BETWEEN, begins_with. `!=`, IN, exists, and anything under OR/NOT
/// are residual-only.
enum KeyOp<'l> {
    Cmp(CmpOp, &'l Lit),
    Between(&'l Lit, &'l Lit),
    BeginsWith(&'l str),
}

fn servable(cond: &Cond) -> Option<(&StmtPath, KeyOp<'_>)> {
    let Cond::Leaf(leaf) = cond else { return None };
    match &leaf.kind {
        LeafKind::Cmp { path, op, lit } if *op != CmpOp::Ne => Some((path, KeyOp::Cmp(*op, lit))),
        LeafKind::Between { path, lo, hi } => Some((path, KeyOp::Between(lo, hi))),
        LeafKind::BeginsWith { path, prefix } => Some((path, KeyOp::BeginsWith(prefix))),
        _ => None,
    }
}

/// Fold every servable conjunct on the chosen index's path into one
/// byte-space interval; return the step and the residual conjunct
/// indices. Single-valued paths intersect (exact — one value makes the
/// conjunction an interval); multi-valued paths take the first
/// equality only (existential semantics — ADR-0080 D1).
fn fold_index_bounds(
    conjuncts: &[Cond],
    spec: &IndexSpec,
    named_explicitly: bool,
) -> Result<(AccessStep, Vec<usize>), QlError> {
    let multi_valued = path_is_multi_valued(spec);
    let mut interval = Interval::unbounded();
    let mut used = vec![false; conjuncts.len()];
    let mut folded = 0usize;
    for (i, conjunct) in conjuncts.iter().enumerate() {
        let Some((path, op)) = servable(conjunct) else { continue };
        if path.program.as_bytes() != spec.program {
            continue;
        }
        if multi_valued {
            match op {
                KeyOp::Cmp(CmpOp::Eq, _) if folded == 0 => {}
                KeyOp::Cmp(CmpOp::Eq, _) => continue, // later equalities re-check as residual
                _ if named_explicitly => {
                    return err(path.at, QlErrorKind::MultiValueRange(name_string(&spec.name)));
                }
                _ => continue, // never a candidate under path matching
            }
        }
        interval = interval.intersect(key_op_interval(spec, path.at, &op)?);
        used[i] = true;
        folded += 1;
    }
    if folded == 0 {
        debug_assert!(named_explicitly, "path matching only chooses constrained indexes");
        return err(0, QlErrorKind::UnconstrainedIndex(name_string(&spec.name)));
    }
    let step = AccessStep::IndexRange {
        index: spec.id,
        generation: spec.generation,
        key_type: spec.key_type,
        lo: interval.lo,
        hi: interval.hi,
    };
    let residual = (0..conjuncts.len()).filter(|&i| !used[i]).collect();
    Ok((step, residual))
}

/// Multi-valued ⇔ the declared path holds a wildcard step. Fence paths
/// without one resolve to ≤ 1 node; `Other` steps cannot pass the
/// registry gauntlet, but read conservatively (multi-valued) anyway.
fn path_is_multi_valued(spec: &IndexSpec) -> bool {
    let program = inf_doc::path::PathProgram::from_bytes(&spec.program)
        .expect("registry programs passed the ADR-0075 gauntlet");
    program.steps().any(|step| {
        debug_assert!(!matches!(step, PathStep::Other), "fenced programs have no Other steps");
        !matches!(step, PathStep::Child(_) | PathStep::Index(_))
    })
}

/// One conjunct → one encoded-byte-space interval (ADR-0080 D3).
fn key_op_interval(spec: &IndexSpec, at: usize, op: &KeyOp<'_>) -> Result<Interval, QlError> {
    match op {
        KeyOp::Cmp(cmp, lit) => lit_interval(spec, at, *cmp, lit),
        KeyOp::Between(lo, hi) => Ok(lit_interval(spec, at, CmpOp::Ge, lo)?
            .intersect(lit_interval(spec, at, CmpOp::Le, hi)?)),
        KeyOp::BeginsWith(prefix) => {
            if spec.key_type != IndexKeyType::Utf8 {
                return err(at, mismatch(spec));
            }
            Ok(prefix_interval(prefix))
        }
    }
}

fn mismatch(spec: &IndexSpec) -> QlErrorKind {
    QlErrorKind::KeyTypeMismatch {
        name: name_string(&spec.name),
        key_type: access::key_type_name(spec.key_type),
    }
}

/// `path ⊙ literal` on a `key_type` index. Key-condition literals are
/// family-strict ({i64, f64} interchange via the ADR-0074 table);
/// residual comparisons keep full D4 semantics — the strictness is the
/// probe's, not the language's.
fn lit_interval(spec: &IndexSpec, at: usize, cmp: CmpOp, lit: &Lit) -> Result<Interval, QlError> {
    debug_assert!(cmp != CmpOp::Ne, "!= is never servable");
    match (spec.key_type, lit) {
        (IndexKeyType::Utf8, Lit::Str(s)) => Ok(utf8_interval(cmp, s)),
        (IndexKeyType::Bool, Lit::Bool(b)) => {
            Ok(same_type_interval(cmp, encode_key(IndexKeyType::Bool, IndexScalar::Bool(*b))))
        }
        (IndexKeyType::I64, Lit::I64(v)) => {
            Ok(same_type_interval(cmp, encode_key(IndexKeyType::I64, IndexScalar::I64(*v))))
        }
        (IndexKeyType::F64, Lit::F64(f)) => {
            Ok(same_type_interval(cmp, encode_key(IndexKeyType::F64, IndexScalar::F64(*f))))
        }
        (IndexKeyType::I64, Lit::F64(f)) => Ok(i64_cross_interval(cmp, *f)),
        (IndexKeyType::F64, Lit::I64(v)) => Ok(f64_cross_interval(cmp, *v)),
        _ => err(at, mismatch(spec)),
    }
}

fn encode_key(key_type: IndexKeyType, value: IndexScalar<'_>) -> Vec<u8> {
    let mut buf = IndexKeyBuf::new();
    index_key_encode(key_type, value, &mut buf).expect("caller admits the value by construction");
    buf.as_bytes().to_vec()
}

/// Same-type bounds need no arithmetic: exclusivity carries strictness.
fn same_type_interval(cmp: CmpOp, key: Vec<u8>) -> Interval {
    match cmp {
        CmpOp::Eq => {
            Interval { lo: RangeEdge::Included(key.clone()), hi: RangeEdge::Included(key) }
        }
        CmpOp::Lt => Interval { lo: RangeEdge::Unbounded, hi: RangeEdge::Excluded(key) },
        CmpOp::Le => Interval { lo: RangeEdge::Unbounded, hi: RangeEdge::Included(key) },
        CmpOp::Gt => Interval { lo: RangeEdge::Excluded(key), hi: RangeEdge::Unbounded },
        CmpOp::Ge => Interval { lo: RangeEdge::Included(key), hi: RangeEdge::Unbounded },
        CmpOp::Ne => unreachable!("!= is never servable"),
    }
}

/// utf8 literals: the encoder refuses over-cap values (they can never
/// have entries — ADR-0074 D3), so equality is empty and inequalities
/// bind at the 1024-byte truncated image with the inclusivity flip
/// that stays exact over storable keys (`k < s ⇔ k ≤ image` when every
/// stored key is shorter than `s` — shorter-is-smaller; ADR-0080 D3).
fn utf8_interval(cmp: CmpOp, s: &str) -> Interval {
    let mut buf = IndexKeyBuf::new();
    if index_key_encode(IndexKeyType::Utf8, IndexScalar::Utf8(s), &mut buf).is_ok() {
        return same_type_interval(cmp, buf.as_bytes().to_vec());
    }
    let mut image = IndexKeyBuf::new();
    let full_len = index_key_escape_prefix(s, &mut image);
    debug_assert!(full_len > ORDERED_KEY_MAX, "encode only refuses over-cap strings");
    let image = image.as_bytes().to_vec();
    match cmp {
        CmpOp::Eq => Interval::empty_fixed(&image),
        CmpOp::Lt | CmpOp::Le => {
            Interval { lo: RangeEdge::Unbounded, hi: RangeEdge::Included(image) }
        }
        CmpOp::Gt | CmpOp::Ge => {
            Interval { lo: RangeEdge::Excluded(image), hi: RangeEdge::Unbounded }
        }
        CmpOp::Ne => unreachable!("!= is never servable"),
    }
}

/// `begins_with` bounds on the ADR-0074 D2 prefix-safety property:
/// lower = the terminator-less prefix image, upper = its byte
/// successor. The empty prefix matches every string; a prefix whose
/// image fills the key cap can have no stored match.
fn prefix_interval(prefix: &str) -> Interval {
    if prefix.is_empty() {
        return Interval::unbounded();
    }
    let mut image_buf = IndexKeyBuf::new();
    let full_len = index_key_escape_prefix(prefix, &mut image_buf);
    if full_len >= ORDERED_KEY_MAX {
        // Any match encodes to image + ≥1 byte > the tree's key cap.
        return Interval::empty_fixed(image_buf.as_bytes());
    }
    let image = image_buf.as_bytes().to_vec();
    let hi = match prefix_successor(&image) {
        Some(successor) => RangeEdge::Excluded(successor),
        // Unreachable for valid UTF-8 (0xFF appears only as an escape
        // pair's second byte, whose first byte is 0x00) — kept total.
        None => RangeEdge::Unbounded,
    };
    Interval { lo: RangeEdge::Included(image), hi }
}

/// Last non-0xFF byte incremented, truncated after (ADR-0074 D2).
fn prefix_successor(image: &[u8]) -> Option<Vec<u8>> {
    let last = image.iter().rposition(|&b| b != 0xFF)?;
    let mut successor = image[..=last].to_vec();
    successor[last] += 1;
    Some(successor)
}

/// i64 index, f64 literal: integrality decides (ADR-0080 D3). The
/// verifying oracle for every branch here is the bound-construction
/// proptest against the production VM.
fn i64_cross_interval(cmp: CmpOp, c: f64) -> Interval {
    debug_assert!(!c.is_nan(), "the lexer only produces finite floats");
    const TWO_POW_63: f64 = 9_223_372_036_854_775_808.0;
    if c >= TWO_POW_63 {
        return match cmp {
            CmpOp::Lt | CmpOp::Le => Interval::unbounded(),
            _ => Interval::empty_fixed(&0i64.to_be_bytes()),
        };
    }
    if c < -TWO_POW_63 {
        return match cmp {
            CmpOp::Gt | CmpOp::Ge => Interval::unbounded(),
            _ => Interval::empty_fixed(&0i64.to_be_bytes()),
        };
    }
    let truncated = c.trunc();
    let t = truncated as i64;
    let integral = c == truncated;
    let key = |v: i64| encode_key(IndexKeyType::I64, IndexScalar::I64(v));
    if integral {
        return same_type_interval(cmp, key(t));
    }
    // Non-integral ⇒ |c| < 2^53 (every f64 ≥ 2^53 is integral), so the
    // ±1 below cannot overflow.
    let ceil = if c > truncated { t + 1 } else { t };
    let floor = if truncated > c { t - 1 } else { t };
    match cmp {
        CmpOp::Eq => Interval::empty_fixed(&key(t)),
        CmpOp::Gt | CmpOp::Ge => {
            Interval { lo: RangeEdge::Included(key(ceil)), hi: RangeEdge::Unbounded }
        }
        CmpOp::Lt | CmpOp::Le => {
            Interval { lo: RangeEdge::Unbounded, hi: RangeEdge::Included(key(floor)) }
        }
        CmpOp::Ne => unreachable!("!= is never servable"),
    }
}

/// f64 index, i64 literal: lossless coercions bind directly; lossy
/// ones bind at the float neighbor, computed as ±1 on the encoded
/// word — the f64 key encoding is a monotone bijection onto its word
/// range, so word arithmetic IS float neighbor stepping (ADR-0080 D3).
fn f64_cross_interval(cmp: CmpOp, c: i64) -> Interval {
    let g = c as f64;
    let lossless = matches!(
        index_scalar_coerce(IndexKeyType::F64, IndexScalar::I64(c)),
        Ok(IndexScalar::F64(_))
    );
    let g_key = encode_key(IndexKeyType::F64, IndexScalar::F64(g));
    if lossless {
        return same_type_interval(cmp, g_key);
    }
    let word = u64::from_be_bytes(g_key.as_slice().try_into().expect("fixed8 key"));
    let side = compare_i64_f64(c, g);
    // Smallest f64 above c / largest below it: the nearest float is on
    // one side; its word-neighbor is on the other (no float lies
    // strictly between a value and its nearest — half-ulp bound).
    let (above, below) = match side {
        Ordering::Less => (word, word - 1),    // c < g
        Ordering::Greater => (word + 1, word), // c > g
        Ordering::Equal => unreachable!("lossy coercion is never equal"),
    };
    match cmp {
        CmpOp::Eq => Interval::empty_fixed(&g_key),
        CmpOp::Gt | CmpOp::Ge => Interval {
            lo: RangeEdge::Included(above.to_be_bytes().to_vec()),
            hi: RangeEdge::Unbounded,
        },
        CmpOp::Lt | CmpOp::Le => Interval {
            lo: RangeEdge::Unbounded,
            hi: RangeEdge::Included(below.to_be_bytes().to_vec()),
        },
        CmpOp::Ne => unreachable!("!= is never servable"),
    }
}

// ---------------------------------------------------------------------
// Byte-space intervals
// ---------------------------------------------------------------------

struct Interval {
    lo: RangeEdge,
    hi: RangeEdge,
}

impl Interval {
    fn unbounded() -> Interval {
        Interval { lo: RangeEdge::Unbounded, hi: RangeEdge::Unbounded }
    }

    /// A statically empty range: `[k, k)` — honest bytes, zero rows at
    /// execution, never an error (reversed BETWEEN is a value — D3).
    fn empty_fixed(key: &[u8]) -> Interval {
        Interval { lo: RangeEdge::Included(key.to_vec()), hi: RangeEdge::Excluded(key.to_vec()) }
    }

    /// Intersection = tighter edge on each side; exact in byte space
    /// because every folded conjunct encodes into the same key order.
    fn intersect(self, other: Interval) -> Interval {
        Interval { lo: tighter_lo(self.lo, other.lo), hi: tighter_hi(self.hi, other.hi) }
    }
}

fn tighter_lo(a: RangeEdge, b: RangeEdge) -> RangeEdge {
    match (&a, &b) {
        (RangeEdge::Unbounded, _) => b,
        (_, RangeEdge::Unbounded) => a,
        (
            RangeEdge::Included(x) | RangeEdge::Excluded(x),
            RangeEdge::Included(y) | RangeEdge::Excluded(y),
        ) => {
            match x.cmp(y) {
                Ordering::Greater => a,
                Ordering::Less => b,
                // Same byte position: exclusion starts later ⇒ tighter.
                Ordering::Equal => {
                    if matches!(a, RangeEdge::Excluded(_)) {
                        a
                    } else {
                        b
                    }
                }
            }
        }
    }
}

fn tighter_hi(a: RangeEdge, b: RangeEdge) -> RangeEdge {
    match (&a, &b) {
        (RangeEdge::Unbounded, _) => b,
        (_, RangeEdge::Unbounded) => a,
        (
            RangeEdge::Included(x) | RangeEdge::Excluded(x),
            RangeEdge::Included(y) | RangeEdge::Excluded(y),
        ) => {
            match x.cmp(y) {
                Ordering::Less => a,
                Ordering::Greater => b,
                // Same byte position: exclusion ends earlier ⇒ tighter.
                Ordering::Equal => {
                    if matches!(a, RangeEdge::Excluded(_)) {
                        a
                    } else {
                        b
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------
// Residual emission (ADR-0079 — this compiler emits that format and
// nothing else)
// ---------------------------------------------------------------------

fn build_residual(
    conjuncts: &[Cond],
    keep: &[usize],
    where_at: usize,
) -> Result<Option<predicate::PredicateProgram>, QlError> {
    if keep.is_empty() {
        return Ok(None);
    }
    let mut parts: Vec<Predicate> = Vec::with_capacity(keep.len());
    for &i in keep {
        parts.push(cond_predicate(&conjuncts[i]));
    }
    let root = and_of(parts);
    let program = predicate::encode(&root).map_err(|e| build_error(e, where_at))?;
    Ok(Some(program))
}

/// Cond → Predicate. Bounded recursion: parse enforces statement
/// nesting ≤ `NESTING_DEPTH_MAX` (32) before this runs, so the stack
/// depth is proven, not hoped (the style carve-out for non-decoder
/// recursion with a bound).
fn cond_predicate(cond: &Cond) -> Predicate {
    match cond {
        Cond::And(children) => and_of(children.iter().map(cond_predicate).collect()),
        Cond::Or(children) => or_of(children.iter().map(cond_predicate).collect()),
        Cond::Not(inner) => Predicate::Not(Box::new(cond_predicate(inner))),
        Cond::Leaf(leaf) => leaf_predicate(leaf),
    }
}

fn leaf_predicate(leaf: &Leaf) -> Predicate {
    match &leaf.kind {
        LeafKind::Cmp { path, op, lit } => {
            Predicate::Cmp { op: *op, path: path.program.clone(), constant: constant(lit) }
        }
        LeafKind::Between { path, lo, hi } => {
            Predicate::Between { path: path.program.clone(), lo: constant(lo), hi: constant(hi) }
        }
        LeafKind::BeginsWith { path, prefix } => {
            Predicate::BeginsWith { path: path.program.clone(), prefix: prefix.clone() }
        }
        LeafKind::In { path, members } => Predicate::In {
            path: path.program.clone(),
            members: members.iter().map(constant).collect(),
        },
        LeafKind::Exists { path } => Predicate::Exists { path: path.program.clone() },
        LeafKind::KeyEq { .. } => unreachable!("$key conjuncts never reach the residual"),
    }
}

fn constant(lit: &Lit) -> Constant {
    match lit {
        Lit::I64(v) => Constant::I64(*v),
        Lit::F64(v) => Constant::F64(*v),
        Lit::Bool(v) => Constant::Bool(*v),
        Lit::Str(s) => Constant::Utf8(s.clone()),
    }
}

/// Flatten to the arity cap, nest beyond it (ADR-0079 D2's "compilers
/// SHOULD flatten"); a single operand collapses.
fn and_of(mut v: Vec<Predicate>) -> Predicate {
    while v.len() > BOOL_ARITY_MAX {
        let tail = v.split_off(v.len() - BOOL_ARITY_MAX);
        v.push(Predicate::And(tail));
    }
    if v.len() == 1 { v.pop().expect("just checked") } else { Predicate::And(v) }
}

fn or_of(mut v: Vec<Predicate>) -> Predicate {
    while v.len() > BOOL_ARITY_MAX {
        let tail = v.split_off(v.len() - BOOL_ARITY_MAX);
        v.push(Predicate::Or(tail));
    }
    if v.len() == 1 { v.pop().expect("just checked") } else { Predicate::Or(v) }
}

fn build_error(e: PredicateBuildError, where_at: usize) -> QlError {
    let kind = match e {
        PredicateBuildError::TooManyOps => QlErrorKind::TooManyOps,
        PredicateBuildError::TooManyPaths => QlErrorKind::TooManyPaths,
        PredicateBuildError::TooManyConstants => QlErrorKind::TooManyConstants,
        PredicateBuildError::TooDeep => QlErrorKind::TooDeep,
        PredicateBuildError::MixedBetweenFamilies => QlErrorKind::MixedBetweenFamilies,
        PredicateBuildError::MixedInFamilies => QlErrorKind::MixedInFamilies,
        PredicateBuildError::ProgramTooLong => QlErrorKind::ProgramTooLong,
        // The lexer keeps floats finite; the parser bounds arity and IN
        // counts; assembled paths are never legacy.
        PredicateBuildError::BadArity
        | PredicateBuildError::BadInCount
        | PredicateBuildError::NonFiniteF64
        | PredicateBuildError::LegacyPath => {
            unreachable!("parse invariants cover {e:?}")
        }
    };
    QlError { offset: where_at, kind }
}

fn name_string(name: &[u8]) -> String {
    String::from_utf8(name.to_vec()).expect("statement names are UTF-8")
}
