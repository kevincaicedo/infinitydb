//! Path-mutation engine v1 (M3-S11/S12; ADR-0041 D5): two-phase
//! plan/apply per milestone §3.4 R4/R5, over plain canonical tape bytes.
//!
//! **Plan** resolves the full match set against the pre-mutation state
//! (canonical order — document order, deduplicated, ADR-0040 D4),
//! validates every per-match operation (type checks, i64 range, non-finite
//! results, the post-edit idoc-byte bound), and computes each edit's exact
//! byte range and replacement. **Apply** then streams one new tape in a
//! single pass. Any validation failure returns before a byte of output
//! exists — there is no rollback path because nothing partial ever starts
//! (R4). Edits that land inside an edited ancestor's range are superseded
//! by it (the reverse-document-order apply semantics of R5, realized as a
//! containment drop — byte-equivalent for every op except `Merge`, where
//! the containment reading IS the pinned contract: ADR-0042 D6) while
//! their per-match results still report against the pre-mutation state.
//!
//! **Backend v1 is a splice-rebuild** — O(document bytes) per command:
//! correct first. The L6 fast path (in-place tape/arena writes, slack
//! growth, node surgery, demand-morph) is M3-S16's story with its own
//! §4.1 budget row; this module's public shape is what S16 optimizes
//! under, and what S17's `DocDelta` replay reuses (the ops below map
//! 1:1 onto delta opcodes; operand fragments are canonical idoc, §3.4
//! R6).
//!
//! Purity fence (§3.4, ADR-0040 D5): `apply` is a pure function of
//! (tape bytes, program, op) — positional iteration only, `BTreeMap` for
//! the patch set (deterministic order), no clocks, no randomness, no
//! hash-map iteration. Replay depends on it (L7).

use std::collections::BTreeMap;

use crate::cursor::DocValue;
use crate::header::HEADER_LEN;
use crate::limits::DOC_BYTES_MAX;
use crate::path::{EvalError, EvalLimits, Matches, PathProgram, SimpleStep, eval};
use crate::tape::{
    Dict, TAG_ARR, TAG_FALSE, TAG_OBJ, TAG_TRUE, TapeDoc, ValueRef, read_u24, read_value,
    skip_value,
};
use crate::{emit, header, merge};

/// A computed JSON number: the i64/f64 split is part of the durable
/// contract (integers preserved exactly — ADR-0036 D4).
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum Number {
    I64(i64),
    F64(f64),
}

/// One path mutation. Operand fragments are canonical idoc **body**
/// bytes (header-less, plain form — §3.4 R6); S17 stores them verbatim
/// in `DocDelta` payloads.
#[derive(Copy, Clone, Debug)]
pub enum ApplyOp<'a> {
    /// `JSON.SET` on an existing-path shape: replace every match with
    /// `fragment`. The program is the full path.
    SetReplace {
        fragment: &'a [u8],
    },
    /// `JSON.SET` whose final segment is a child name: the program is
    /// the **parent** path; each matched object replaces the member if
    /// `key` exists or appends it (insertion order — ADR-0036 D5).
    /// Non-object parents are skipped (the oracle-pinned rule).
    SetMember {
        key: &'a [u8],
        fragment: &'a [u8],
    },
    /// `JSON.DEL` on a non-root path: remove every match (object
    /// members drop key + value; array elements shift left).
    Del,
    NumIncrBy(Number),
    NumMultBy(Number),
    /// Decoded UTF-8 payload of the JSON-string operand.
    StrAppend(&'a [u8]),
    Toggle,
    Clear,
    /// `JSON.ARRAPPEND` (M3-S13): the operand is one canonical **array**
    /// fragment whose elements are the argv values in order (ADR-0042
    /// D2 — one operand per future `DocDelta`); its element bytes splice
    /// onto the end of every matched array.
    ArrAppend {
        elements: &'a [u8],
    },
    /// `JSON.ARRINSERT`: `ArrAppend` at `index`. Negatives resolve
    /// against the length; the legal resolved range is `0..=len` — out
    /// of range aborts the whole command (§3.4 R4, ADR-0042 D3).
    ArrInsert {
        index: i64,
        elements: &'a [u8],
    },
    /// `JSON.ARRPOP`: negatives resolve, then clamp to `[0, len-1]`
    /// (out of range rounds to the nearest end); an empty array is the
    /// non-mutating `PoppedEmpty` arm (ADR-0042 D3).
    ArrPop {
        index: i64,
    },
    /// `JSON.ARRTRIM`: keep the inclusive `[start, stop]` window after
    /// negative resolution + clamping; never a range error (ADR-0042
    /// D3).
    ArrTrim {
        start: i64,
        stop: i64,
    },
    /// `JSON.MERGE` (M3-S14): RFC 7386 at the matched value; only null
    /// members inside an object patch delete members (ADR-0042 D6). The
    /// operand is the patch as a canonical fragment (§3.4 R6).
    Merge {
        patch: &'a [u8],
    },
}

/// Per-match outcome, in **raw (program) order** — replies echo raw
/// order (ADR-0040 D4) even though edits apply in document order.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum MatchResult {
    /// Type mismatch for the op (or a no-op `CLEAR`): untouched,
    /// reported as the oracle's `null`/uncounted arm.
    Skipped,
    /// New numeric value (`NUMINCRBY`/`NUMMULTBY`).
    Num(Number),
    /// New string length in bytes (`STRAPPEND` — S21 pins the oracle's
    /// unit).
    Len(u64),
    /// New boolean (`TOGGLE`).
    Toggled(bool),
    Cleared,
    Removed,
    Set,
    /// Popped element's byte offset into the **pre-mutation** body
    /// (`JSON.ARRPOP`): the caller resolves it against the frozen
    /// pre-image via [`TapeDoc::value_at`] — offsets are meaningful only
    /// against the exact document this apply ran on (ADR-0042 D4).
    Popped(u32),
    /// `JSON.ARRPOP` on an empty array: a real array match that pops
    /// nothing — no edit, no version bump, `null` reply element.
    PoppedEmpty,
}

/// The apply verdict. `bytes` is the complete new document (header +
/// body) — `None` when every match was skipped: nothing changed, the
/// caller must not rewrite the record or bump the version (ADR-0041 D8).
#[derive(Clone, Debug)]
pub struct ApplyOutcome {
    pub bytes: Option<Vec<u8>>,
    pub results: Vec<MatchResult>,
    /// Count of non-skipped matches (the `JSON.DEL`/`JSON.CLEAR` reply).
    pub applied: u32,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ApplyError {
    /// i64 arithmetic left the representable range (plan phase — nothing
    /// mutated; the R4 contract).
    Overflow,
    /// An f64 result was NaN/±Inf (idoc numbers are finite).
    NotANumber,
    /// The post-edit document would exceed the configured byte cap.
    TooLarge,
    /// An `ARRINSERT` index fell outside `0..=len` after negative
    /// resolution (plan phase — nothing mutated; the R4 contract).
    OutOfBounds,
    /// Root deletion is key lifecycle and must use the kernel `Delete`
    /// effect; it is never a document delta (ADR-0043 D5).
    RootDelete,
    Eval(EvalError),
}

/// Result of the ADR-0043 same-width scalar probe. `Unsupported` means
/// the canonical two-phase engine must run; every other arm is a complete
/// command verdict and the probe has either committed exactly once or
/// left the document untouched.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum ScalarPatch {
    Unsupported,
    Missing,
    Skipped,
    Number(Number),
    Toggled(bool),
}

/// Allocation-free same-width scalar patch over a plain stored tape.
/// Only simple Child/Index paths enter; all other shapes fall through
/// before a byte changes (ADR-0043 D1).
pub fn patch_scalar_in_place(
    idoc: &mut [u8],
    program: &PathProgram,
    op: &ApplyOp<'_>,
) -> Result<ScalarPatch, ApplyError> {
    if idoc.get(3).copied().unwrap_or(0) != 0 {
        return Ok(ScalarPatch::Unsupported);
    }
    let Some(steps) = program.simple_steps() else {
        return Ok(ScalarPatch::Unsupported);
    };
    let body = &idoc[HEADER_LEN..];
    let Some(at) = locate_simple(body, steps) else {
        return Ok(ScalarPatch::Missing);
    };
    match *op {
        ApplyOp::NumIncrBy(operand) | ApplyOp::NumMultBy(operand) => {
            let (value, end) = read_value(body, Dict::empty(), at);
            let (ValueRef::I64(_) | ValueRef::F64(_)) = value else {
                return Ok(ScalarPatch::Skipped);
            };
            let result = num_op(value, operand, matches!(op, ApplyOp::NumMultBy(_)))?;
            let mut encoded = [0u8; emit::I64_MAX_LEN];
            let len = encode_number(result, &mut encoded);
            if len != end - at {
                return Ok(ScalarPatch::Unsupported);
            }
            idoc[HEADER_LEN + at..HEADER_LEN + end].copy_from_slice(&encoded[..len]);
            Ok(ScalarPatch::Number(result))
        }
        ApplyOp::Toggle => match body[at] {
            TAG_FALSE => {
                idoc[HEADER_LEN + at] = TAG_TRUE;
                Ok(ScalarPatch::Toggled(true))
            }
            TAG_TRUE => {
                idoc[HEADER_LEN + at] = TAG_FALSE;
                Ok(ScalarPatch::Toggled(false))
            }
            _ => Ok(ScalarPatch::Skipped),
        },
        _ => Ok(ScalarPatch::Unsupported),
    }
}

fn locate_simple<'a>(body: &[u8], steps: impl Iterator<Item = SimpleStep<'a>>) -> Option<usize> {
    let mut at = 0usize;
    for step in steps {
        at = match step {
            SimpleStep::Child(key) => locate_child(body, at, key)?,
            SimpleStep::Index(index) => locate_index(body, at, index)?,
        };
    }
    Some(at)
}

fn locate_child(body: &[u8], at: usize, key: &[u8]) -> Option<usize> {
    if body[at] != TAG_OBJ {
        return None;
    }
    let end = skip_value(body, at);
    let mut off = at + 4;
    while off < end {
        let (ValueRef::Str(candidate), value_at) = read_value(body, Dict::empty(), off) else {
            unreachable!("validated object keys are strings")
        };
        if candidate.as_bytes() == key {
            return Some(value_at);
        }
        off = skip_value(body, value_at);
    }
    None
}

fn locate_index(body: &[u8], at: usize, index: i64) -> Option<usize> {
    if body[at] != TAG_ARR {
        return None;
    }
    let len = arr_len(body, at);
    let index = resolve_index(index, len);
    if !(0..len as i64).contains(&index) {
        return None;
    }
    Some(arr_nth_offset(body, at, index as usize))
}

fn encode_number(number: Number, out: &mut [u8; emit::I64_MAX_LEN]) -> usize {
    match number {
        Number::F64(value) => {
            let encoded: &mut [u8; emit::F64_LEN] =
                (&mut out[..emit::F64_LEN]).try_into().expect("i64 scratch fits f64 encoding");
            emit::f64_into(encoded, value);
            emit::F64_LEN
        }
        Number::I64(value) => emit::i64_into(out, value),
    }
}

impl core::fmt::Display for ApplyError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ApplyError::Overflow => write!(f, "arithmetic overflows a 64-bit integer"),
            ApplyError::NotANumber => write!(f, "result is not a number"),
            ApplyError::TooLarge => write!(f, "document too large"),
            ApplyError::OutOfBounds => write!(f, "index out of bounds"),
            ApplyError::RootDelete => write!(f, "document root deletion requires a key delete"),
            ApplyError::Eval(e) => write!(f, "{e}"),
        }
    }
}

impl core::error::Error for ApplyError {}

impl From<EvalError> for ApplyError {
    fn from(e: EvalError) -> ApplyError {
        ApplyError::Eval(e)
    }
}

/// Apply `op` at every location `program` matches in `doc`.
///
/// `doc` must be the **plain** canonical form (never interned — the
/// store's freeze path guarantees it; ADR-0038 D3 keeps interning
/// storage-local). `max_body` is the namespace's idoc-byte cap
/// (clamped to the format ceiling like `ParseLimits`).
pub fn apply(
    doc: &TapeDoc<'_>,
    program: &PathProgram,
    op: &ApplyOp<'_>,
    limits: &EvalLimits,
    max_body: usize,
) -> Result<ApplyOutcome, ApplyError> {
    if program.is_root() && matches!(op, ApplyOp::Del) {
        return Err(ApplyError::RootDelete);
    }
    #[cfg(feature = "doc-intern-keys")]
    debug_assert!(doc.dict().is_empty(), "apply requires the plain canonical form (ADR-0038 D3)");
    let body = doc.body();
    let matches = eval(program, DocValue::from(doc.root()), limits)?;
    let canon = matches.canonical();
    let sites = locate_all(body, &matches, &canon.ids);
    debug_assert_eq!(sites.len(), canon.ids.len(), "every match locates on its own tape");

    let mut plan = Plan::default();
    for site in &sites {
        let verdict = plan_site(body, site, op)?;
        plan.push(verdict);
    }
    let results = raw_order_results(&matches, &canon.ids, &plan.results);
    let applied = results.iter().filter(|r| !matches!(r, MatchResult::Skipped)).count() as u32;
    let kept = drop_superseded(&mut plan.edits);
    if kept.is_empty() {
        return Ok(ApplyOutcome { bytes: None, results, applied });
    }
    let bytes = build_output(body, &kept, max_body.min(DOC_BYTES_MAX))?;
    Ok(ApplyOutcome { bytes: Some(bytes), results, applied })
}

/// Wrap parsed value fragments into the single canonical **array**
/// operand `ARRAPPEND`/`ARRINSERT` carry (ADR-0042 D2 — one operand per
/// future `DocDelta`). `None` when the combined operand would exceed the
/// u24/document ceiling (the command layer answers the size error).
pub fn array_operand(fragments: &[&[u8]]) -> Option<Vec<u8>> {
    debug_assert!(!fragments.is_empty(), "command arity guarantees at least one value");
    let body: usize = fragments.iter().map(|f| f.len()).sum();
    if body > DOC_BYTES_MAX - 4 {
        return None;
    }
    let mut out = Vec::with_capacity(4 + body);
    out.push(TAG_ARR);
    out.extend_from_slice(&(body as u32).to_le_bytes()[..3]);
    for fragment in fragments {
        out.extend_from_slice(fragment);
    }
    Some(out)
}

/// The `JSON.MERGE` creation value (ADR-0042 D6): `MergePatch(absent,
/// patch)` — object patches merge into `{}` (nulls stripped through
/// object chains), everything else is literal — as a complete document
/// (header + body), ready for `json_set`.
pub fn merge_absent_document(patch_fragment: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; HEADER_LEN];
    merge::merge_absent(patch_fragment, &mut out);
    let body_len = (out.len() - HEADER_LEN) as u32;
    header::patch(&mut out, 0, body_len);
    debug_assert!(TapeDoc::from_bytes(&out).is_ok(), "merge emits canonical documents");
    out
}

// ---- plan ------------------------------------------------------------------

/// One located match on the tape: byte extents plus the enclosing
/// container headers whose u24 lengths an edit here must re-cover.
struct Site {
    /// Entry start: the key tag for object members, `value_start`
    /// otherwise — `Del` removes the whole entry.
    entry_start: u32,
    value_start: u32,
    value_end: u32,
    /// Container header offsets from root to parent (ascending).
    ancestors: Vec<u32>,
}

/// One planned byte edit. `depth` orders same-offset inserts:
/// a deeper container's append must land before its ancestor's
/// (both extend the same byte position; the deeper bytes belong to the
/// inner extent).
struct Edit {
    start: u32,
    end: u32,
    depth: u16,
    repl: Vec<u8>,
    /// Headers to grow/shrink by this edit's delta (site ancestors, plus
    /// the container itself for member inserts).
    patch: Vec<u32>,
}

#[derive(Default)]
struct Plan {
    results: Vec<MatchResult>,
    edits: Vec<Edit>,
}

enum Verdict {
    Skip,
    Apply { result: MatchResult, edit: Option<Edit> },
}

impl Plan {
    fn push(&mut self, verdict: Verdict) {
        match verdict {
            Verdict::Skip => self.results.push(MatchResult::Skipped),
            Verdict::Apply { result, edit } => {
                self.results.push(result);
                if let Some(e) = edit {
                    self.edits.push(e);
                }
            }
        }
    }
}

/// Validate one op against one pre-mutation site and compute its edit.
/// Every error path returns before any output byte exists (§3.4 R4).
fn plan_site(body: &[u8], site: &Site, op: &ApplyOp<'_>) -> Result<Verdict, ApplyError> {
    let at = site.value_start as usize;
    let value = read_value(body, Dict::empty(), at).0;
    let replace = |repl: Vec<u8>, result: MatchResult| Verdict::Apply {
        result,
        edit: Some(Edit {
            start: site.value_start,
            end: site.value_end,
            depth: site.ancestors.len() as u16,
            repl,
            patch: site.ancestors.clone(),
        }),
    };
    Ok(match op {
        ApplyOp::SetReplace { fragment } => replace(fragment.to_vec(), MatchResult::Set),
        ApplyOp::SetMember { key, fragment } => {
            let ValueRef::Obj(_) = value else { return Ok(Verdict::Skip) };
            return Ok(plan_member_set(body, site, key, fragment));
        }
        ApplyOp::Del => Verdict::Apply {
            result: MatchResult::Removed,
            edit: Some(Edit {
                start: site.entry_start,
                end: site.value_end,
                depth: site.ancestors.len() as u16,
                repl: Vec::new(),
                patch: site.ancestors.clone(),
            }),
        },
        ApplyOp::NumIncrBy(operand) | ApplyOp::NumMultBy(operand) => {
            let (ValueRef::I64(_) | ValueRef::F64(_)) = value else { return Ok(Verdict::Skip) };
            let mul = matches!(op, ApplyOp::NumMultBy(_));
            let result = num_op(value, *operand, mul)?;
            let mut repl = Vec::with_capacity(emit::I64_MAX_LEN);
            match result {
                Number::I64(v) => emit::i64(&mut repl, v),
                Number::F64(v) => emit::f64(&mut repl, v),
            }
            replace(repl, MatchResult::Num(result))
        }
        ApplyOp::StrAppend(payload) => {
            let ValueRef::Str(s) = value else { return Ok(Verdict::Skip) };
            let old = s.as_bytes();
            let new_len = old.len() + payload.len();
            let mut repl = Vec::with_capacity(4 + new_len);
            emit::str_header(&mut repl, new_len);
            repl.extend_from_slice(old);
            repl.extend_from_slice(payload);
            replace(repl, MatchResult::Len(new_len as u64))
        }
        ApplyOp::Toggle => {
            let ValueRef::Bool(b) = value else { return Ok(Verdict::Skip) };
            replace(vec![if b { TAG_FALSE } else { TAG_TRUE }], MatchResult::Toggled(!b))
        }
        ApplyOp::Clear => {
            let repl = match value {
                ValueRef::Obj(o) if !o.is_empty() => vec![TAG_OBJ, 0, 0, 0],
                ValueRef::Arr(a) if !a.is_empty() => vec![TAG_ARR, 0, 0, 0],
                // Numbers zero to the canonical integer 0 (fixint); the
                // already-clear arms skip — a no-op edit must not bump
                // versions or (S17) log records (ADR-0041 D8).
                ValueRef::I64(v) if v != 0 => vec![0x00],
                ValueRef::F64(f) if f != 0.0 => vec![0x00],
                _ => return Ok(Verdict::Skip),
            };
            replace(repl, MatchResult::Cleared)
        }
        ApplyOp::ArrAppend { elements } => {
            let ValueRef::Arr(_) = value else { return Ok(Verdict::Skip) };
            return Ok(plan_arr_splice(body, site, arr_len(body, at), elements));
        }
        ApplyOp::ArrInsert { index, elements } => {
            let ValueRef::Arr(_) = value else { return Ok(Verdict::Skip) };
            let len = arr_len(body, at);
            let resolved = resolve_index(*index, len);
            if !(0..=len as i64).contains(&resolved) {
                return Err(ApplyError::OutOfBounds);
            }
            return Ok(plan_arr_splice(body, site, resolved as usize, elements));
        }
        ApplyOp::ArrPop { index } => {
            let ValueRef::Arr(_) = value else { return Ok(Verdict::Skip) };
            let len = arr_len(body, at);
            if len == 0 {
                return Ok(Verdict::Apply { result: MatchResult::PoppedEmpty, edit: None });
            }
            // Out-of-range indices round to the nearest end (ADR-0042 D3).
            let resolved = resolve_index(*index, len).clamp(0, len as i64 - 1) as usize;
            let element_start = arr_nth_offset(body, at, resolved);
            let element_end = skip_value(body, element_start);
            Verdict::Apply {
                result: MatchResult::Popped(element_start as u32),
                edit: Some(Edit {
                    start: element_start as u32,
                    end: element_end as u32,
                    depth: site.ancestors.len() as u16,
                    repl: Vec::new(),
                    patch: with_self(&site.ancestors, site.value_start),
                }),
            }
        }
        ApplyOp::ArrTrim { start, stop } => {
            let ValueRef::Arr(_) = value else { return Ok(Verdict::Skip) };
            return Ok(plan_arr_trim(body, site, *start, *stop));
        }
        ApplyOp::Merge { patch } => {
            // Overlap contract (ADR-0042 D6): every site's merge is a
            // pure function of the pre-mutation snapshot, and a site
            // contained in a changed ancestor is superseded by it — for
            // Merge the containment drop IS the semantics, not merely a
            // reverse-order equivalence (an ancestor's merge depends on
            // descendant values, so the two readings genuinely differ).
            //
            let mut repl = Vec::new();
            merge::merge_value(body, at, patch, &mut repl);
            if repl[..] == body[at..site.value_end as usize] {
                // Byte-equal merge (`{}` into anything): the ADR-0041 D8
                // no-op discipline — uncounted, no rewrite, no version.
                return Ok(Verdict::Skip);
            }
            replace(repl, MatchResult::Set)
        }
    })
}

/// Splice an array operand's element bytes in before element `position`
/// (`position == len` appends). The operand is the ADR-0042 D2 canonical
/// array fragment.
fn plan_arr_splice(body: &[u8], site: &Site, position: usize, elements: &[u8]) -> Verdict {
    debug_assert_eq!(elements[0], TAG_ARR, "the operand is a canonical array fragment");
    let at = site.value_start as usize;
    let insert_at = arr_nth_offset(body, at, position);
    let added = arr_len(elements, 0);
    debug_assert!(added > 0, "command arity guarantees at least one value");
    let element_bytes = &elements[4..skip_value(elements, 0)];
    Verdict::Apply {
        result: MatchResult::Len((arr_len(body, at) + added) as u64),
        edit: Some(Edit {
            start: insert_at as u32,
            end: insert_at as u32,
            depth: site.ancestors.len() as u16 + 1,
            repl: element_bytes.to_vec(),
            patch: with_self(&site.ancestors, site.value_start),
        }),
    }
}

/// `ARRTRIM` keeps the inclusive `[start, stop]` window after negative
/// resolution and clamping — out-of-range never errors (ADR-0042 D3);
/// a window covering the whole array is a no-op (ADR-0041 D8).
///
/// Overlap contract (ADR-0042 D6): the kept window copies
/// **pre-mutation** bytes, so — like `Merge` — a matched descendant
/// inside an edited trim is superseded by it; the containment drop is
/// the pinned semantics, not a reverse-order equivalence.
fn plan_arr_trim(body: &[u8], site: &Site, start: i64, stop: i64) -> Verdict {
    let at = site.value_start as usize;
    let len = arr_len(body, at);
    let no_edit =
        |kept: usize| Verdict::Apply { result: MatchResult::Len(kept as u64), edit: None };
    if len == 0 {
        return no_edit(0);
    }
    let first = resolve_index(start, len).max(0);
    let last = resolve_index(stop, len).min(len as i64 - 1);
    if first == 0 && last == len as i64 - 1 {
        return no_edit(len);
    }
    let mut repl = vec![TAG_ARR, 0, 0, 0];
    let mut kept = 0usize;
    if first <= last {
        let keep_start = arr_nth_offset(body, at, first as usize);
        let keep_end = arr_nth_offset(body, at, last as usize + 1);
        repl.extend_from_slice(&body[keep_start..keep_end]);
        let body_len = (keep_end - keep_start) as u32;
        repl[1..4].copy_from_slice(&body_len.to_le_bytes()[..3]);
        kept = (last - first + 1) as usize;
    }
    Verdict::Apply {
        result: MatchResult::Len(kept as u64),
        edit: Some(Edit {
            start: site.value_start,
            end: site.value_end,
            depth: site.ancestors.len() as u16,
            repl,
            patch: site.ancestors.clone(),
        }),
    }
}

/// Negative array indices resolve against the length (the RedisJSON
/// rule); callers decide clamping vs rejection per op (ADR-0042 D3).
fn resolve_index(index: i64, len: usize) -> i64 {
    if index < 0 { index + len as i64 } else { index }
}

/// Element count of the array value at `at` (tape arrays store no count
/// — ADR-0036 D3; the walk is O(elements), plan-phase only).
fn arr_len(body: &[u8], at: usize) -> usize {
    debug_assert_eq!(body[at], TAG_ARR);
    let end = skip_value(body, at);
    let mut off = at + 4;
    let mut count = 0;
    while off < end {
        off = skip_value(body, off);
        count += 1;
    }
    count
}

/// Byte offset of element `n` of the array at `at` (`n == len` is the
/// end-of-body append position).
fn arr_nth_offset(body: &[u8], at: usize, n: usize) -> usize {
    let mut off = at + 4;
    for _ in 0..n {
        off = skip_value(body, off);
    }
    off
}

/// `SetMember` on one matched parent object: replace the first entry
/// whose key matches (the pinned first-match rule, ADR-0036 D5) or
/// append a new entry at the end of the object body.
fn plan_member_set(body: &[u8], site: &Site, key: &[u8], fragment: &[u8]) -> Verdict {
    let hdr = site.value_start as usize;
    debug_assert_eq!(body[hdr], TAG_OBJ, "caller matched an object");
    let end = site.value_end as usize;
    let mut off = hdr + 4;
    while off < end {
        let (entry_key, val_at) = match read_value(body, Dict::empty(), off) {
            (ValueRef::Str(s), next) => (s, next),
            _ => unreachable!("validated object key positions hold strings"),
        };
        let val_end = skip_value(body, val_at);
        if entry_key.as_bytes() == key {
            return Verdict::Apply {
                result: MatchResult::Set,
                edit: Some(Edit {
                    start: val_at as u32,
                    end: val_end as u32,
                    depth: site.ancestors.len() as u16 + 1,
                    repl: fragment.to_vec(),
                    patch: with_self(&site.ancestors, site.value_start),
                }),
            };
        }
        off = val_end;
    }
    let mut repl = Vec::with_capacity(4 + key.len() + fragment.len());
    emit::str(&mut repl, key);
    repl.extend_from_slice(fragment);
    Verdict::Apply {
        result: MatchResult::Set,
        edit: Some(Edit {
            start: end as u32,
            end: end as u32,
            depth: site.ancestors.len() as u16 + 1,
            repl,
            patch: with_self(&site.ancestors, site.value_start),
        }),
    }
}

fn with_self(ancestors: &[u32], own: u32) -> Vec<u32> {
    let mut v = Vec::with_capacity(ancestors.len() + 1);
    v.extend_from_slice(ancestors);
    v.push(own);
    v
}

/// RedisJSON numeric model (ADR-0041 D8): i64 while both sides are
/// integers and the result is in range; f64 otherwise. i64 overflow and
/// non-finite f64 results abort the whole command — the oracle's error
/// arms, pinned `oracle-pending` until S21's container byte-diffs them.
fn num_op(value: ValueRef<'_>, operand: Number, mul: bool) -> Result<Number, ApplyError> {
    let current = match value {
        ValueRef::I64(v) => Number::I64(v),
        ValueRef::F64(v) => Number::F64(v),
        _ => unreachable!("caller checked the numeric arms"),
    };
    number_op(current, operand, mul)
}

pub(crate) fn number_op(current: Number, operand: Number, mul: bool) -> Result<Number, ApplyError> {
    if let (Number::I64(a), Number::I64(b)) = (current, operand) {
        let exact = if mul { a.checked_mul(b) } else { a.checked_add(b) };
        return exact.map(Number::I64).ok_or(ApplyError::Overflow);
    }
    let a = match current {
        Number::I64(v) => v as f64,
        Number::F64(v) => v,
    };
    let b = match operand {
        Number::I64(v) => v as f64,
        Number::F64(v) => v,
    };
    let result = if mul { a * b } else { a + b };
    if !result.is_finite() {
        return Err(ApplyError::NotANumber);
    }
    Ok(Number::F64(result))
}

// ---- locate ----------------------------------------------------------------

/// Locate every canonical match's byte extents in one document-order
/// walk. `canon_ids` index into `matches` sorted by location path —
/// exactly the order a depth-first tape walk visits them, so one pass
/// suffices (iterative, depth-bounded by the validated tape).
/// One open container during the locate walk.
struct Open {
    hdr: u32,
    end: u32,
    is_obj: bool,
    next_ord: u32,
}

/// The root value (empty location path) spans the whole body; every
/// other match requires the root to be a container to descend into.
/// Returns the first unlocated target index, or `None` when the walk
/// below has nothing left to find.
fn locate_root(
    body: &[u8],
    matches: &Matches,
    canon_ids: &[u32],
    sites: &mut Vec<Site>,
) -> Option<usize> {
    let mut k = 0usize;
    if k < canon_ids.len() && matches.get(canon_ids[k] as usize).is_empty() {
        sites.push(Site {
            entry_start: 0,
            value_start: 0,
            value_end: body.len() as u32,
            ancestors: Vec::new(),
        });
        k += 1;
    }
    if k == canon_ids.len() || !matches!(body[0], TAG_OBJ | TAG_ARR) {
        debug_assert_eq!(k, canon_ids.len(), "matches exist only inside containers");
        return None;
    }
    Some(k)
}

fn locate_all(body: &[u8], matches: &Matches, canon_ids: &[u32]) -> Vec<Site> {
    let mut sites: Vec<Site> = Vec::with_capacity(canon_ids.len());
    let Some(mut k) = locate_root(body, matches, canon_ids, &mut sites) else {
        return sites;
    };
    let target = |k: usize| matches.get(canon_ids[k] as usize);
    let mut stack: Vec<Open> =
        vec![Open { hdr: 0, end: body.len() as u32, is_obj: body[0] == TAG_OBJ, next_ord: 0 }];
    let mut path: Vec<u32> = Vec::new();
    let mut off = 4usize; // past the root container header
    while k < canon_ids.len() {
        let top = stack.last_mut().expect("targets are real locations, so a frame remains");
        if off as u32 == top.end {
            stack.pop();
            path.pop();
            continue;
        }
        let ord = top.next_ord;
        top.next_ord += 1;
        let is_obj = top.is_obj;
        let entry_start = off;
        let value_start = if is_obj { skip_value(body, off) } else { off };
        let value_end = skip_value(body, value_start);
        let t = target(k);
        let matches_here =
            t.len() == path.len() + 1 && t[..path.len()] == path[..] && t[path.len()] == ord;
        if matches_here {
            let ancestors = stack.iter().map(|o| o.hdr).collect();
            sites.push(Site {
                entry_start: entry_start as u32,
                value_start: value_start as u32,
                value_end: value_end as u32,
                ancestors,
            });
            k += 1;
        }
        // Descend when the next unlocated target lies inside this value
        // (a strictly deeper path continuing through this ordinal) —
        // matches nest, so a located site may still need descending into.
        let descend = k < canon_ids.len() && {
            let t = target(k);
            t.len() > path.len() + 1 && t[..path.len()] == path[..] && t[path.len()] == ord
        };
        if descend {
            debug_assert!(matches!(body[value_start], TAG_OBJ | TAG_ARR), "targets are real");
            stack.push(Open {
                hdr: value_start as u32,
                end: value_end as u32,
                is_obj: body[value_start] == TAG_OBJ,
                next_ord: 0,
            });
            path.push(ord);
            off = value_start + 4;
        } else {
            off = value_end;
        }
    }
    sites
}

// ---- apply -----------------------------------------------------------------

/// Sort edits into stream order and drop those contained in an edited
/// ancestor's range — the containment realization of reverse-document-
/// order apply (module docs): the ancestor's replacement supersedes
/// every edit inside it, byte-for-byte.
fn drop_superseded(edits: &mut Vec<Edit>) -> Vec<Edit> {
    // Ascending start; same-offset inserts deepest-first (module docs on
    // `Edit::depth`).
    use core::cmp::Reverse;
    edits.sort_by_key(|e| (e.start, Reverse(e.depth)));
    let mut kept: Vec<Edit> = Vec::with_capacity(edits.len());
    let mut covered_end = 0u32;
    let mut covering = false;
    for edit in edits.drain(..) {
        if covering && edit.start < covered_end {
            debug_assert!(edit.end <= covered_end, "value extents nest, never straddle");
            continue;
        }
        if edit.end > edit.start {
            covered_end = edit.end;
            covering = true;
        }
        kept.push(edit);
    }
    kept
}

/// Stream the new document in one pass: gaps memcpy, ancestor headers
/// re-cover their new body length, edit ranges emit their replacement.
fn build_output(body: &[u8], kept: &[Edit], max_body: usize) -> Result<Vec<u8>, ApplyError> {
    // Per-header delta sums (BTreeMap: deterministic order, offsets are
    // walk order). Only kept edits contribute — a superseded edit's
    // bytes vanish inside its ancestor's replacement.
    let mut patches: BTreeMap<u32, i64> = BTreeMap::new();
    let mut total: i64 = 0;
    for edit in kept {
        let delta = edit.repl.len() as i64 - (edit.end - edit.start) as i64;
        total += delta;
        if delta == 0 {
            continue;
        }
        for &hdr in &edit.patch {
            *patches.entry(hdr).or_insert(0) += delta;
        }
    }
    let new_len = body.len() as i64 + total;
    debug_assert!(new_len > 0, "a document keeps a root value");
    if new_len as usize > max_body {
        return Err(ApplyError::TooLarge);
    }
    // Drop headers inside kept edit ranges (their subtree is replaced
    // wholesale) and zero-delta entries.
    let mut out = Vec::with_capacity(HEADER_LEN + new_len as usize);
    out.extend_from_slice(&[0u8; HEADER_LEN]);
    let mut pos = 0usize;
    let mut edit_iter = kept.iter().peekable();
    for (&hdr, &delta) in &patches {
        if delta == 0 {
            continue;
        }
        // Emit every edit that starts before this header.
        while let Some(e) = edit_iter.peek() {
            if e.start > hdr {
                break;
            }
            let e = edit_iter.next().expect("peeked");
            out.extend_from_slice(&body[pos..e.start as usize]);
            out.extend_from_slice(&e.repl);
            pos = e.end as usize;
        }
        debug_assert!(pos <= hdr as usize, "patched headers lie outside kept edit ranges");
        out.extend_from_slice(&body[pos..hdr as usize]);
        let old = read_u24(body, hdr as usize + 1) as i64;
        let new = old + delta;
        debug_assert!((0..=DOC_BYTES_MAX as i64).contains(&new), "u24 bound follows the doc cap");
        out.push(body[hdr as usize]);
        out.extend_from_slice(&(new as u32).to_le_bytes()[..3]);
        pos = hdr as usize + 4;
    }
    for e in edit_iter {
        out.extend_from_slice(&body[pos..e.start as usize]);
        out.extend_from_slice(&e.repl);
        pos = e.end as usize;
    }
    out.extend_from_slice(&body[pos..]);
    debug_assert_eq!(out.len() - HEADER_LEN, new_len as usize, "delta arithmetic is exact");
    header::patch(&mut out, 0, new_len as u32);
    debug_assert!(TapeDoc::from_bytes(&out).is_ok(), "apply emits canonical documents");
    Ok(out)
}

// ---- results ---------------------------------------------------------------

/// Map canonical-order results back onto the raw match order (replies
/// echo raw order — ADR-0040 D4). Duplicate raw matches (union members
/// selecting one node) share their canonical twin's result.
fn raw_order_results(
    matches: &Matches,
    canon_ids: &[u32],
    canon_results: &[MatchResult],
) -> Vec<MatchResult> {
    debug_assert_eq!(canon_ids.len(), canon_results.len());
    let mut results = vec![MatchResult::Skipped; matches.len()];
    for (i, &raw_id) in canon_ids.iter().enumerate() {
        results[raw_id as usize] = canon_results[i];
    }
    // Duplicates were deduplicated out of the canonical set; find each
    // one's twin by location path (binary search — canon is sorted).
    if canon_ids.len() != matches.len() {
        for (raw, result) in results.iter_mut().enumerate() {
            let twin =
                canon_ids.binary_search_by(|&id| matches.get(id as usize).cmp(matches.get(raw)));
            if let Ok(pos) = twin {
                *result = canon_results[pos];
            }
        }
    }
    results
}
