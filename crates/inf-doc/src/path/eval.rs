//! Deterministic path evaluation (M3-S09; ADR-0040 D3–D6).
//!
//! A frame-stack DFS over [`DocValue`] cursors: each frame applies one
//! op to one node and iterates its selections; pops emit **program
//! order** (document order within every selector, member order across
//! union members, step order for slices — the frozen raw order, D4).
//! Matches are identified by **location paths** — entry-ordinal steps
//! from the root — the form-agnostic identity that makes document-order
//! sorting, dedup, and §3.4 R5 prefix-overlap detection pure integer
//! work (D3).
//!
//! Bounded everything (L9/L5): a caller budget counts visited nodes and
//! yields an **owned, borrow-free** [`EvalState`] on exhaustion (D6 —
//! cursors never cross a suspension boundary; resume re-derives them);
//! the match set is capped with a typed error.

use crate::cursor::{ArrEntries, DocValue, ObjEntries};

use super::PathProgram;
use super::program::{Op, read_op};

/// Evaluation bounds (ADR-0040 D6). `max_matches` is a product limit
/// (documented at S22): the pathological `$..*` over a 16 MiB document
/// otherwise builds an unbounded plan set.
#[derive(Copy, Clone, Debug)]
pub struct EvalLimits {
    pub max_matches: u32,
}

impl Default for EvalLimits {
    fn default() -> EvalLimits {
        EvalLimits { max_matches: 65_536 }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum EvalError {
    /// Match set exceeded `EvalLimits::max_matches`.
    TooManyMatches,
}

impl core::fmt::Display for EvalError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            EvalError::TooManyMatches => write!(f, "path matched too many values"),
        }
    }
}

impl core::error::Error for EvalError {}

/// The match set: location paths flattened into one arena (no per-match
/// allocation). Raw order is the frozen program order (ADR-0040 D4);
/// [`Matches::canonical`] derives the document-order view §3.4 R5
/// consumes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Matches {
    steps: Vec<u32>,
    spans: Vec<Span>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct Span {
    at: u32,
    len: u16,
}

impl Matches {
    pub fn len(&self) -> usize {
        self.spans.len()
    }

    pub fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }

    /// Location path of match `i`, in raw (program) order.
    pub fn get(&self, i: usize) -> &[u32] {
        let span = self.spans[i];
        &self.steps[span.at as usize..span.at as usize + span.len as usize]
    }

    pub fn iter(&self) -> impl Iterator<Item = &[u32]> {
        (0..self.len()).map(|i| self.get(i))
    }

    /// Document-order view (§3.4 R5): match ids sorted by location path
    /// (lexicographic — document order on both physical forms),
    /// deduplicated, plus the ancestor/descendant overlap verdict the
    /// mutation planner branches on. Overlap detection is an adjacent
    /// prefix check: in lexicographic order every strict ancestor sorts
    /// immediately into its descendant run.
    pub fn canonical(&self) -> CanonicalMatches {
        let mut ids: Vec<u32> = (0..self.spans.len() as u32).collect();
        ids.sort_by(|&a, &b| self.get(a as usize).cmp(self.get(b as usize)));
        ids.dedup_by(|&mut a, &mut b| self.get(a as usize) == self.get(b as usize));
        let mut any_overlap = false;
        for pair in ids.windows(2) {
            let shorter = self.get(pair[0] as usize);
            let longer = self.get(pair[1] as usize);
            if longer.len() > shorter.len() && &longer[..shorter.len()] == shorter {
                any_overlap = true;
                break;
            }
        }
        CanonicalMatches { ids, any_overlap }
    }

    fn record(&mut self, path: &[u32], step: Option<u32>, cap: u32) -> Result<(), EvalError> {
        let len = path.len() + usize::from(step.is_some());
        if self.spans.len() as u32 == cap {
            return Err(EvalError::TooManyMatches);
        }
        debug_assert!(len <= u16::MAX as usize, "path depth is bounded far below u16");
        let at = self.steps.len() as u32;
        self.steps.extend_from_slice(path);
        if let Some(s) = step {
            self.steps.push(s);
        }
        self.spans.push(Span { at, len: len as u16 });
        Ok(())
    }
}

/// [`Matches::canonical`]'s result: ids into the raw set, document
/// order, deduplicated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CanonicalMatches {
    pub ids: Vec<u32>,
    pub any_overlap: bool,
}

/// Resolve a location path against a root — O(depth) hops (each hop an
/// ordinal walk within one container).
pub fn resolve<'a>(root: DocValue<'a>, steps: &[u32]) -> Option<DocValue<'a>> {
    let mut node = root;
    for &step in steps {
        node = match node {
            DocValue::Obj(o) => o.iter().nth(step as usize)?.1,
            DocValue::Arr(a) => a.index(step as usize)?,
            _ => return None,
        };
    }
    Some(node)
}

/// One live frame: `op_at` applies to `node`; selections continue at
/// `next_pc`. `stepped` marks whether entering this frame pushed one
/// step onto the shared path trail.
struct Frame<'a> {
    op_at: u32,
    next_pc: u32,
    node: DocValue<'a>,
    stepped: bool,
    progress: Progress<'a>,
}

enum Progress<'a> {
    Fresh,
    /// One-shot op already fired.
    Done,
    Obj {
        it: ObjEntries<'a>,
        next_ord: u32,
    },
    Arr {
        it: ArrEntries<'a>,
        next_ord: u32,
    },
    Slice {
        next: i64,
        stop: i64,
        step: i64,
    },
    Union {
        member: u8,
    },
    /// Descend: the self item was emitted; children iterate below.
    DescendObj {
        it: ObjEntries<'a>,
        next_ord: u32,
    },
    DescendArr {
        it: ArrEntries<'a>,
        next_ord: u32,
    },
}

/// A produced work item: apply the op at `pc` to `node` (`pc == end`
/// means `node` is a match); `cont` is that op's continuation.
struct Item<'a> {
    node: DocValue<'a>,
    pc: u32,
    cont: u32,
    step: Option<u32>,
}

pub enum EvalStep {
    Done(Matches),
    /// Budget exhausted: owned state, **no document borrows** (the
    /// INFINITY_STYLE suspension rule) — resume via [`eval_budgeted`].
    Yield(Box<EvalState>),
}

/// Owned suspension state (ADR-0040 D6): frame progress as counters,
/// the step trail, and matches so far. Cursors are re-derived on resume
/// by replaying the trail from the root — O(depth × container width)
/// once per yield, not per node.
#[derive(Clone, Debug)]
pub struct EvalState {
    frames: Vec<SavedFrame>,
    path: Vec<u32>,
    matches: Matches,
}

#[derive(Copy, Clone, Debug)]
struct SavedFrame {
    op_at: u32,
    next_pc: u32,
    stepped: bool,
    progress: SavedProgress,
}

#[derive(Copy, Clone, Debug)]
enum SavedProgress {
    Fresh,
    Done,
    Iter { next_ord: u32 },
    Slice { next: i64, stop: i64, step: i64 },
    Union { member: u8 },
    DescendIter { next_ord: u32 },
}

/// Evaluate with an unbounded budget. In debug builds the whole run is
/// executed twice and compared — the S09 determinism invariant, absent
/// from release hot paths (the DST fleet consumes [`Matches`] equality
/// directly).
pub fn eval<'a>(
    program: &PathProgram,
    root: DocValue<'a>,
    limits: &EvalLimits,
) -> Result<Matches, EvalError> {
    let result = run(program, root, limits, u64::MAX, None)?;
    let EvalStep::Done(matches) = result else {
        unreachable!("unbounded budget cannot yield");
    };
    #[cfg(debug_assertions)]
    {
        let EvalStep::Done(again) = run(program, root, limits, u64::MAX, None)? else {
            unreachable!("unbounded budget cannot yield");
        };
        debug_assert_eq!(matches, again, "evaluation is deterministic (L7)");
    }
    Ok(matches)
}

/// Evaluate under a node budget; `resume` continues a prior yield.
/// The budget is decremented once per visited node (the §4.1 `Descend`
/// yield row: the caller's slice budget is M).
pub fn eval_budgeted<'a>(
    program: &PathProgram,
    root: DocValue<'a>,
    limits: &EvalLimits,
    budget_nodes: u64,
    resume: Option<Box<EvalState>>,
) -> Result<EvalStep, EvalError> {
    run(program, root, limits, budget_nodes, resume)
}

fn run<'a>(
    program: &PathProgram,
    root: DocValue<'a>,
    limits: &EvalLimits,
    mut budget: u64,
    resume: Option<Box<EvalState>>,
) -> Result<EvalStep, EvalError> {
    let bytes = program.as_bytes();
    let end = bytes.len() as u32;
    let mut path: Vec<u32>;
    let mut matches: Matches;
    let mut stack: Vec<Frame<'a>>;
    match resume {
        Some(state) => {
            let state = *state;
            path = state.path;
            matches = state.matches;
            stack = rebuild_frames(&state.frames, &path, root);
        }
        None => {
            path = Vec::new();
            matches = Matches::default();
            stack = Vec::new();
            // The Root op (offset 2) selects the root node once.
            let item = Item { node: root, pc: 3, cont: cont_of(bytes, 3), step: None };
            consume(item, &mut stack, &mut path, &mut matches, end, limits)?;
        }
    }
    loop {
        let Some(top) = stack.last_mut() else {
            return Ok(EvalStep::Done(matches));
        };
        let Some(item) = advance(top, bytes, end) else {
            let frame = stack.pop().expect("stack top exists");
            if frame.stepped {
                path.pop();
            }
            continue;
        };
        if budget == 0 {
            // Re-stage the item: rewinding the parent iterator is not
            // possible, so the item becomes a Fresh frame first — the
            // saved state then resumes exactly here.
            consume(item, &mut stack, &mut path, &mut matches, end, limits)?;
            return Ok(EvalStep::Yield(Box::new(save(&stack, &path, &matches))));
        }
        budget -= 1;
        consume(item, &mut stack, &mut path, &mut matches, end, limits)?;
    }
}

/// Record a finished item as a match, or push its frame.
fn consume<'a>(
    item: Item<'a>,
    stack: &mut Vec<Frame<'a>>,
    path: &mut Vec<u32>,
    matches: &mut Matches,
    end: u32,
    limits: &EvalLimits,
) -> Result<(), EvalError> {
    if item.pc == end {
        return matches.record(path, item.step, limits.max_matches);
    }
    if let Some(step) = item.step {
        path.push(step);
    }
    stack.push(Frame {
        op_at: item.pc,
        next_pc: item.cont,
        node: item.node,
        stepped: item.step.is_some(),
        progress: Progress::Fresh,
    });
    Ok(())
}

/// Continuation of the op at `pc` (== `end` when `pc` is the last op).
fn cont_of(bytes: &[u8], pc: u32) -> u32 {
    if pc as usize >= bytes.len() {
        return bytes.len() as u32;
    }
    read_op(bytes, pc as usize).1 as u32
}

/// Advance one frame by one selection. `None` ⇒ the frame is exhausted.
fn advance<'a>(frame: &mut Frame<'a>, bytes: &[u8], end: u32) -> Option<Item<'a>> {
    let (op, _) = read_op(bytes, frame.op_at as usize);
    match op {
        Op::Child(key) => {
            if !matches!(frame.progress, Progress::Fresh) {
                return None;
            }
            frame.progress = Progress::Done;
            let DocValue::Obj(o) = frame.node else { return None };
            let (ord, value) = o
                .iter()
                .enumerate()
                .find(|(_, (k, _))| k.as_bytes() == key)
                .map(|(ord, (_, v))| (ord as u32, v))?;
            Some(child_item(frame, bytes, end, value, ord))
        }
        Op::Index(i) => {
            if !matches!(frame.progress, Progress::Fresh) {
                return None;
            }
            frame.progress = Progress::Done;
            let DocValue::Arr(a) = frame.node else { return None };
            let ord = resolve_index(i, || a.len() as i64)?;
            let value = a.index(ord as usize)?;
            Some(child_item(frame, bytes, end, value, ord))
        }
        Op::ChildAny => {
            if let Progress::Fresh = frame.progress {
                frame.progress = match frame.node {
                    DocValue::Obj(o) => Progress::Obj { it: o.iter(), next_ord: 0 },
                    DocValue::Arr(a) => Progress::Arr { it: a.iter(), next_ord: 0 },
                    _ => Progress::Done,
                };
            }
            match &mut frame.progress {
                Progress::Obj { it, next_ord } => {
                    let (_, value) = it.next()?;
                    let ord = *next_ord;
                    *next_ord += 1;
                    Some(child_item(frame, bytes, end, value, ord))
                }
                Progress::Arr { it, next_ord } => {
                    let value = it.next()?;
                    let ord = *next_ord;
                    *next_ord += 1;
                    Some(child_item(frame, bytes, end, value, ord))
                }
                _ => None,
            }
        }
        Op::Slice(spec) => {
            if let Progress::Fresh = frame.progress {
                let DocValue::Arr(a) = frame.node else {
                    frame.progress = Progress::Done;
                    return None;
                };
                let (next, stop, step) = resolve_slice(&spec, a.len() as i64);
                frame.progress = Progress::Slice { next, stop, step };
            }
            let Progress::Slice { next, stop, step } = &mut frame.progress else {
                return None;
            };
            if (*step > 0 && *next >= *stop) || (*step < 0 && *next <= *stop) {
                return None;
            }
            let ord = *next;
            *next += *step;
            debug_assert!(ord >= 0, "resolved slice indices are in range");
            let DocValue::Arr(a) = frame.node else { unreachable!("slice progress on array") };
            let value = a.index(ord as usize).expect("resolved slice indices are in range");
            Some(child_item(frame, bytes, end, value, ord as u32))
        }
        Op::Union(u) => {
            let member = match &mut frame.progress {
                Progress::Fresh => {
                    frame.progress = Progress::Union { member: 0 };
                    0
                }
                Progress::Union { member } => *member,
                _ => return None,
            };
            if member == u.count {
                return None;
            }
            frame.progress = Progress::Union { member: member + 1 };
            // The member op applies to this same node; its continuation
            // is past the whole union (ADR-0040 D4 member order).
            Some(Item {
                node: frame.node,
                pc: u.member_at(member) as u32,
                cont: frame.next_pc,
                step: None,
            })
        }
        Op::Descend => {
            let sel_at = frame.op_at + 1;
            match &mut frame.progress {
                Progress::Fresh => {
                    // Pre-order: the node itself first (document order).
                    frame.progress = match frame.node {
                        DocValue::Obj(o) => Progress::DescendObj { it: o.iter(), next_ord: 0 },
                        DocValue::Arr(a) => Progress::DescendArr { it: a.iter(), next_ord: 0 },
                        _ => Progress::Done,
                    };
                    Some(Item {
                        node: frame.node,
                        pc: sel_at,
                        cont: cont_of(bytes, sel_at),
                        step: None,
                    })
                }
                Progress::DescendObj { it, next_ord } => {
                    let (_, value) = it.next()?;
                    let ord = *next_ord;
                    *next_ord += 1;
                    // The child recurses the descend op itself.
                    Some(Item { node: value, pc: frame.op_at, cont: sel_at, step: Some(ord) })
                }
                Progress::DescendArr { it, next_ord } => {
                    let value = it.next()?;
                    let ord = *next_ord;
                    *next_ord += 1;
                    Some(Item { node: value, pc: frame.op_at, cont: sel_at, step: Some(ord) })
                }
                _ => None,
            }
        }
        Op::Root => unreachable!("Root never becomes a frame"),
    }
}

fn child_item<'a>(
    frame: &Frame<'a>,
    bytes: &[u8],
    _end: u32,
    value: DocValue<'a>,
    ord: u32,
) -> Item<'a> {
    Item { node: value, pc: frame.next_pc, cont: cont_of(bytes, frame.next_pc), step: Some(ord) }
}

/// Negative-index resolution: `len` is fetched lazily (a tape-array
/// `len()` walks the body — only paid when the index is negative).
fn resolve_index(i: i64, len: impl FnOnce() -> i64) -> Option<u32> {
    if i >= 0 {
        return Some(i as u32); // out-of-range surfaces as `index() == None`
    }
    let resolved = i + len();
    if resolved < 0 { None } else { Some(resolved as u32) }
}

/// Grammar §4: Python slice index resolution, pinned.
fn resolve_slice(spec: &super::ast::SliceSpec, len: i64) -> (i64, i64, i64) {
    let step = spec.step.unwrap_or(1);
    debug_assert_ne!(step, 0, "validator rejects step 0");
    let resolve = |v: i64| if v < 0 { v + len } else { v };
    if step > 0 {
        let start = spec.start.map(resolve).unwrap_or(0).clamp(0, len);
        let stop = spec.end.map(resolve).unwrap_or(len).clamp(0, len);
        (start, stop, step)
    } else {
        let start = spec.start.map(resolve).unwrap_or(len - 1).clamp(-1, len - 1);
        let stop = spec.end.map(resolve).unwrap_or(-1).clamp(-1, len - 1);
        (start, stop, step)
    }
}

fn save(stack: &[Frame<'_>], path: &[u32], matches: &Matches) -> EvalState {
    let frames = stack
        .iter()
        .map(|f| SavedFrame {
            op_at: f.op_at,
            next_pc: f.next_pc,
            stepped: f.stepped,
            progress: match &f.progress {
                Progress::Fresh => SavedProgress::Fresh,
                Progress::Done => SavedProgress::Done,
                Progress::Obj { next_ord, .. } | Progress::Arr { next_ord, .. } => {
                    SavedProgress::Iter { next_ord: *next_ord }
                }
                Progress::Slice { next, stop, step } => {
                    SavedProgress::Slice { next: *next, stop: *stop, step: *step }
                }
                Progress::Union { member } => SavedProgress::Union { member: *member },
                Progress::DescendObj { next_ord, .. } | Progress::DescendArr { next_ord, .. } => {
                    SavedProgress::DescendIter { next_ord: *next_ord }
                }
            },
        })
        .collect();
    EvalState { frames, path: path.to_vec(), matches: matches.clone() }
}

/// Re-derive live frames from saved counters: each frame's node comes
/// from its parent's node plus the step it pushed (the path trail holds
/// them in stack order); iterators are recreated and fast-forwarded.
fn rebuild_frames<'a>(saved: &[SavedFrame], path: &[u32], root: DocValue<'a>) -> Vec<Frame<'a>> {
    let mut frames = Vec::with_capacity(saved.len());
    let mut node = root;
    let mut step_idx = 0usize;
    for sf in saved {
        if sf.stepped {
            let step = path[step_idx];
            step_idx += 1;
            node = resolve(node, &[step]).expect("saved path resolves against the same document");
        }
        let progress = match sf.progress {
            SavedProgress::Fresh => Progress::Fresh,
            SavedProgress::Done => Progress::Done,
            SavedProgress::Iter { next_ord } => match node {
                DocValue::Obj(o) => {
                    let mut it = o.iter();
                    for _ in 0..next_ord {
                        it.next().expect("saved ordinal within container");
                    }
                    Progress::Obj { it, next_ord }
                }
                DocValue::Arr(a) => {
                    let mut it = a.iter();
                    for _ in 0..next_ord {
                        it.next().expect("saved ordinal within container");
                    }
                    Progress::Arr { it, next_ord }
                }
                _ => unreachable!("iterating frame on a container"),
            },
            SavedProgress::Slice { next, stop, step } => Progress::Slice { next, stop, step },
            SavedProgress::Union { member } => Progress::Union { member },
            SavedProgress::DescendIter { next_ord } => match node {
                DocValue::Obj(o) => {
                    let mut it = o.iter();
                    for _ in 0..next_ord {
                        it.next().expect("saved ordinal within container");
                    }
                    Progress::DescendObj { it, next_ord }
                }
                DocValue::Arr(a) => {
                    let mut it = a.iter();
                    for _ in 0..next_ord {
                        it.next().expect("saved ordinal within container");
                    }
                    Progress::DescendArr { it, next_ord }
                }
                _ => unreachable!("descend-iterating frame on a container"),
            },
        };
        frames.push(Frame {
            op_at: sf.op_at,
            next_pc: sf.next_pc,
            node,
            stepped: sf.stepped,
            progress,
        });
    }
    debug_assert_eq!(step_idx, path.len(), "every path step belongs to a frame");
    frames
}
