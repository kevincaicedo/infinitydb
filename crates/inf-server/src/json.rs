//! `JSON.*` command family (M3-S11/S12; ADR-0041): argv → path programs
//! (through the per-cell S10 cache) → document reads/mutations → RESP
//! replies.
//!
//! Reply shapes follow ADR-0041 D7: `$`-mode reads/replies wrap match
//! sets (`JSON.GET` in JSON text; `STRAPPEND`-class in RESP arrays with
//! nulls for skipped matches); legacy paths answer single values — first
//! match for reads, last applied for mutations — and error when nothing
//! matches. Optional paths default to legacy root `.` (the RedisJSON v1
//! heritage). Every shape and error string here is pinned locally; the
//! S21 redis-stack corpus byte-diffs both protocols, and every accepted
//! divergence lives in its checked allowlist.
//!
//! Mutations validate before they apply, end-to-end: `inf_doc::apply`
//! validates the whole match set before producing bytes, and the store
//! rewrite (`json_replace` — one version bump) happens only when an edit
//! actually applied. A failed command leaves value, version, and
//! accounting untouched; a no-op command (all matches skipped) never
//! rewrites.
//!
//! Every reply is written through one [`JsonReply`] and charged to one
//! `doc-max-reply-bytes` account (ADR-0099 A1). A path mutation builds its
//! whole reply inside [`commit_delta`] before its commit, so a reply over
//! the account refuses before anything changed; a reply known only after
//! its effect reserves its fixed shape's maximum before that effect.

mod reply;

use std::num::NonZeroU32;

use inf_doc::apply::{ApplyError, ApplyOp, ApplyOutcome, MatchResult, Number, apply};
use inf_doc::path::{EvalLimits, Matches, PathProgram, Segment, eval, resolve};
use inf_doc::ser::{Reply, ReplyTooLarge, SerializeOpts};
use inf_doc::{CanonicalDoc, DeltaOpcode, DocValue, ObjCursor, TapeDoc, encode_apply_op};
use inf_foundation::time::Nanos;
use inf_store::{
    CellStore, JsonRead, JsonScalarPatch, JsonSetOptions, JsonSetOutcome, OpError, SetCond,
    SetExpire,
};
use inf_wire::{CommandId, Protocol, RespWriter};

use crate::exec::{Argv, ConnCx, parse_i64};
use crate::limits::FixedShape;
use reply::{
    FixedReservation, FixedValue, JsonReply, ReplyBuild, ReplyError, ReplyStop, ReplyVerdict,
    Settled,
};

/// Mutating members of the family. The durable plane uses this metadata
/// classification for full-image admission before the command runs.
pub(crate) fn is_json_write(id: CommandId) -> bool {
    matches!(
        id,
        CommandId::JsonSet
            | CommandId::JsonDel
            | CommandId::JsonForget
            | CommandId::JsonNumIncrBy
            | CommandId::JsonNumMultBy
            | CommandId::JsonStrAppend
            | CommandId::JsonToggle
            | CommandId::JsonClear
            | CommandId::JsonArrAppend
            | CommandId::JsonArrInsert
            | CommandId::JsonArrPop
            | CommandId::JsonArrTrim
            | CommandId::JsonMerge
    )
}

/// One command's logical document effect. The cell is single-threaded and
/// consumes this scratch immediately after execution; owned operands are
/// encoded once into a recycled buffer, while `PathProgram` clones share
/// immutable `Rc` bytes (ADR-0043 D2).
#[derive(Debug, Default)]
pub(crate) struct DocLogScratch {
    pub(crate) intent: DocLogIntent,
    pub(crate) operand: Vec<u8>,
}

#[derive(Debug, Default)]
pub(crate) enum DocLogIntent {
    #[default]
    None,
    Delete,
    Full,
    Delta {
        program: PathProgram,
        opcode: DeltaOpcode,
        /// A delta record counts at least one match: the log decoder
        /// refuses zero (ADR-0043 D3).
        match_count: NonZeroU32,
    },
}

impl DocLogScratch {
    pub(crate) fn clear(&mut self) {
        self.intent = DocLogIntent::None;
        self.operand.clear();
    }

    #[inline]
    pub(crate) fn bytes(&self) -> usize {
        self.operand.capacity()
    }

    fn delete(&mut self) {
        self.intent = DocLogIntent::Delete;
        self.operand.clear();
    }

    fn full(&mut self) {
        self.intent = DocLogIntent::Full;
        self.operand.clear();
    }

    fn delta(&mut self, program: &PathProgram, op: &ApplyOp<'_>, match_count: NonZeroU32) {
        let opcode = encode_apply_op(op, &mut self.operand);
        self.intent = DocLogIntent::Delta { program: program.clone(), opcode, match_count };
    }
}

#[inline]
fn capture_full(cx: &ConnCx) {
    if cx.node.doc_log_admission.get().is_some() {
        cx.node.doc_log.borrow_mut().full();
    }
}

#[inline]
fn capture_delete(cx: &ConnCx) {
    if cx.node.doc_log_admission.get().is_some() {
        cx.node.doc_log.borrow_mut().delete();
    }
}

#[inline]
fn capture_delta(cx: &ConnCx, program: &PathProgram, op: &ApplyOp<'_>, match_count: NonZeroU32) {
    if cx.node.doc_log_admission.get().is_some() {
        cx.node.doc_log.borrow_mut().delta(program, op, match_count);
    }
}

/// Exact late admission for an already-planned canonical post-image.
/// The ordinary argv/current-image estimate runs before execution; this
/// check handles the only shape it cannot bound tightly without running
/// the semantic planner first: one operand replicated over many matches.
/// It runs before store commit, so refusal leaves state/version/cadence
/// untouched. The expiry record is reserved conservatively even when the
/// current document has no TTL. A refusal is returned, not written: the
/// caller's reply rolls back before the line is answered (ADR-0099 A1).
fn durable_full_fits(cx: &ConnCx, key: &[u8], idoc: &[u8]) -> Result<(), ReplyError<'static>> {
    let Some(admission) = cx.node.doc_log_admission.get() else {
        return Ok(());
    };
    // Cleared for non-JSON writes and numbered dbs across two files; a
    // miss here means nothing to fit, never a cell panic.
    let Some(ns) = cx.ns.named() else { return Ok(()) };
    let full = inf_log::RecordView::DocFull {
        ns,
        key,
        lineage: inf_log::DocLineage::FIRST,
        version: 0,
        idoc,
    }
    .encoded_len();
    let expiry = inf_log::RecordView::ExpireAt { ns, at_unix_ms: u64::MAX, key }.encoded_len();
    if full > admission.record_max {
        return Err(ReplyError::Line("ERR document too large for durable log staging"));
    }
    if full.saturating_add(expiry) > admission.budget {
        // Counted with the owner-side `would_fit` refusals
        // (`log_admission_busy`): same typed reply, same
        // invisible-to-staging pre-check shape.
        cx.node.log_admission_busy.set(cx.node.log_admission_busy.get() + 1);
        return Err(ReplyError::Line(crate::durable::STAGING_BUSY_ERROR));
    }
    Ok(())
}

#[allow(clippy::wildcard_enum_match_arm, reason = "ADR-0143: column handler")]
pub(crate) fn execute_json(
    id: CommandId,
    argv: &(impl Argv + ?Sized),
    store: &mut CellStore,
    cx: &ConnCx,
    now: Nanos,
    w: &mut RespWriter<'_>,
) {
    let reply = JsonReply::open(w, store.doc_max_reply_bytes());
    let settled = match id {
        CommandId::JsonSet => set(argv, store, cx, now, reply),
        CommandId::JsonGet => get(argv, store, cx, now, reply),
        CommandId::JsonMget => mget(argv, store, cx, now, reply),
        CommandId::JsonDel | CommandId::JsonForget => del(argv, store, cx, now, reply),
        CommandId::JsonType => type_of(argv, store, cx, now, reply),
        CommandId::JsonNumIncrBy | CommandId::JsonNumMultBy => {
            num_op(id, argv, store, cx, now, reply)
        }
        CommandId::JsonStrAppend => str_append(argv, store, cx, now, reply),
        CommandId::JsonStrLen => str_len(argv, store, cx, now, reply),
        CommandId::JsonToggle => toggle(argv, store, cx, now, reply),
        CommandId::JsonClear => clear(argv, store, cx, now, reply),
        CommandId::JsonArrAppend => arr_append(argv, store, cx, now, reply),
        CommandId::JsonArrInsert => arr_insert(argv, store, cx, now, reply),
        CommandId::JsonArrIndex => arr_index(argv, store, cx, now, reply),
        CommandId::JsonArrLen => arr_len(argv, store, cx, now, reply),
        CommandId::JsonArrPop => arr_pop(argv, store, cx, now, reply),
        CommandId::JsonArrTrim => arr_trim(argv, store, cx, now, reply),
        CommandId::JsonObjKeys => obj_keys(argv, store, cx, now, reply),
        CommandId::JsonObjLen => obj_len(argv, store, cx, now, reply),
        CommandId::JsonMerge => merge(argv, store, cx, now, reply),
        CommandId::JsonDebug => debug(argv, store, now, reply),
        _ => unreachable!("execute_db routes exactly the JSON family here"),
    };
    debug_assert!(
        settled.refusals() == 0 || settled.verdict() != ReplyVerdict::Declined,
        "a declined reply refused nothing"
    );
    // The one site that folds a reply's refusal figures into this cell's
    // counters (ADR-0099 A1). Both saturate: they are lifetime metrics,
    // and no accounting reads them.
    let node = &cx.node;
    let refusals = node.json_reply_refusals_cell.get();
    node.json_reply_refusals_cell.set(refusals.saturating_add(settled.refusals()));
    let refused_bytes = node.json_reply_refused_bytes_cell.get();
    node.json_reply_refused_bytes_cell.set(refused_bytes.saturating_add(settled.refused_bytes()));
}

// ---- JSON.DEBUG -------------------------------------------------------------

fn debug(
    argv: &(impl Argv + ?Sized),
    store: &mut CellStore,
    now: Nanos,
    reply: JsonReply<'_, '_>,
) -> Settled {
    if !argv.arg(1).eq_ignore_ascii_case(b"MEMORY") {
        return reply.decline(ReplyError::Line("ERR unknown JSON.DEBUG subcommand"));
    }
    match store.json_memory_usage(argv.arg(2), now) {
        Ok(Some(bytes)) => {
            let bytes = i64::try_from(bytes).expect("one document is bounded far below i64::MAX");
            answer(reply, |reply| reply.int(bytes))
        }
        Ok(None) => answer(reply, JsonReply::null),
        Err(error) => reply.decline(ReplyError::Op(error)),
    }
}

// ---- shared plumbing --------------------------------------------------------

/// A reply that is one charged write and no effect.
fn answer<'w, 'b>(
    mut reply: JsonReply<'w, 'b>,
    write: impl FnOnce(&mut JsonReply<'w, 'b>) -> Result<(), ReplyTooLarge>,
) -> Settled {
    let built = write(&mut reply).map_err(ReplyStop::from);
    reply.settle(built)
}

/// Compile through the per-cell cache (S10), cloning the program out of
/// the cache borrow. `PathProgram` owns cell-local `Rc` bytes, so a cache
/// hit clone is allocation-free without holding the `RefCell` guard across
/// later command work (ADR-0043 D1).
fn compile(
    store: &CellStore,
    cx: &ConnCx,
    text: &[u8],
) -> Result<PathProgram, ReplyError<'static>> {
    let mut cache = cx.node.path_cache.borrow_mut();
    match cache.get_or_compile(text, store.doc_max_path_bytes()) {
        Ok(program) => Ok(program.clone()),
        Err(error) => Err(ReplyError::Path(error)),
    }
}

fn eval_limits(store: &CellStore) -> EvalLimits {
    EvalLimits { max_matches: store.doc_max_path_matches() }
}

/// Parse a JSON value argument with the target store's resolved limits
/// (ADR-0039 D5's per-namespace resolution) into the recycled per-cell
/// ingest buffer (the S05 lever-G seam). The receipt borrows `out` and is
/// what a store sink takes (ADR-0169 D4).
fn parse_value<'o>(
    store: &CellStore,
    cx: &ConnCx,
    text: &[u8],
    out: &'o mut Vec<u8>,
) -> Result<CanonicalDoc<'o>, ReplyError<'static>> {
    let mut parser = cx.node.json_parser.borrow_mut();
    parser.set_limits(store.doc_parse_limits());
    parser.parse_into(text, out).map_err(ReplyError::Json)
}

const MISSING_KEY: &str = "ERR could not perform this operation on a key that doesn't exist";
const CREATE_AT_ROOT: &str = "ERR new objects must be created at the root";

/// A document read for a read command: `None` when the key is missing.
fn read_doc<'s>(
    store: &'s mut CellStore,
    key: &[u8],
    now: Nanos,
) -> Result<Option<JsonRead<'s>>, ReplyError<'static>> {
    store.json_get(key, now).map_err(ReplyError::Op)
}

/// Freeze a document's plain canonical bytes for a path mutation; a
/// missing key answers `missing`. The freeze copy is the interim ADR-0041
/// D5 backend (S16 owns the in-place fast path).
fn frozen_doc(
    store: &mut CellStore,
    key: &[u8],
    now: Nanos,
    missing: &'static str,
) -> Result<Vec<u8>, ReplyError<'static>> {
    match store.json_freeze(key, now) {
        Ok(Some(bytes)) => Ok(bytes),
        Ok(None) => Err(ReplyError::Line(missing)),
        Err(error) => Err(ReplyError::Op(error)),
    }
}

/// One planned path edit, ready to commit.
struct PathEdit<'e> {
    key: &'e [u8],
    program: &'e PathProgram,
    op: &'e ApplyOp<'e>,
    outcome: &'e ApplyOutcome,
}

/// A path edit's commit step (ADR-0099 A1). The refusal is returned, not
/// written: the built reply rolls back before the line is answered.
enum CommitOutcome {
    Committed,
    /// No match applied: no bytes, no version bump, no record.
    NoEdit,
    Refused(ReplyError<'static>),
}

/// Commit a mutation outcome: the staging admission, then the rewrite and
/// one version bump when an edit applied (ADR-0041 D8).
fn commit(store: &mut CellStore, edit: &PathEdit<'_>, cx: &ConnCx, now: Nanos) -> CommitOutcome {
    let Some(document) = &edit.outcome.document else { return CommitOutcome::NoEdit };
    if let Err(error) = durable_full_fits(cx, edit.key, document.as_bytes()) {
        return CommitOutcome::Refused(error);
    }
    match store.json_replace(edit.key, document, now) {
        Ok(replaced) => {
            debug_assert!(replaced, "the key was resolved by the freeze above");
            // A delta's match count is a nonzero u32 replay witness
            // (ADR-0043 D3). A match set that cannot be one logs the full
            // post-image, the other record D5 allows for a path edit and
            // the one the fit check above admitted.
            let match_count = u32::try_from(edit.outcome.results.len()).ok();
            match match_count.and_then(NonZeroU32::new) {
                Some(match_count) => capture_delta(cx, edit.program, edit.op, match_count),
                None => capture_full(cx),
            }
            CommitOutcome::Committed
        }
        Err(error) => CommitOutcome::Refused(ReplyError::Op(error)),
    }
}

/// Build a path mutation's whole reply, then commit its edit (ADR-0099 A1's
/// transition table): a builder that refuses or declines leaves no effect,
/// and a commit refusal rolls the built reply back and answers its own
/// line. No-op outcomes remain absent from the log, matching the version
/// rule: no bytes, no bump, no record.
fn commit_delta<'w, 'b, 'a>(
    mut reply: JsonReply<'w, 'b>,
    edit: &PathEdit<'_>,
    store: &mut CellStore,
    cx: &ConnCx,
    now: Nanos,
    build: impl FnOnce(&mut JsonReply<'w, 'b>) -> ReplyBuild<'a>,
) -> Settled {
    // The planted canary runs the effect before the builder; the reply
    // oracle must see a refusal that changed the document.
    let effect_first = cfg!(inf_canary_json_reply_after_effect);
    let early = effect_first.then(|| commit(store, edit, cx, now));
    if let Err(stop) = build(&mut reply) {
        return reply.settle(Err(stop));
    }
    let outcome = match early {
        Some(outcome) => outcome,
        None => commit(store, edit, cx, now),
    };
    match outcome {
        CommitOutcome::Committed | CommitOutcome::NoEdit => reply.finish(),
        CommitOutcome::Refused(error) => reply.decline(error),
    }
}

/// The last applied (non-skipped) match in raw order — the legacy
/// single-value mutation reply (ADR-0041 D7; S21 oracle-verified).
fn last_applied(results: &[MatchResult]) -> Option<MatchResult> {
    results.iter().rev().find(|r| !matches!(r, MatchResult::Skipped)).copied()
}

// ---- JSON.SET ---------------------------------------------------------------

fn set(
    argv: &(impl Argv + ?Sized),
    store: &mut CellStore,
    cx: &ConnCx,
    now: Nanos,
    reply: JsonReply<'_, '_>,
) -> Settled {
    let cond = match argv.len() {
        4 => SetCond::Always,
        5 if argv.arg(4).eq_ignore_ascii_case(b"NX") => SetCond::IfAbsent,
        5 if argv.arg(4).eq_ignore_ascii_case(b"XX") => SetCond::IfPresent,
        _ => return reply.decline(ReplyError::Line("ERR syntax error")),
    };
    let (key, path) = (argv.arg(1), argv.arg(2));
    let program = match compile(store, cx, path) {
        Ok(program) => program,
        Err(error) => return reply.decline(error),
    };
    let mut buf = cx.node.json_ingest_buf.take();
    let settled = match parse_value(store, cx, argv.arg(3), &mut buf) {
        Ok(idoc) if program.is_root() => set_root(reply, store, key, cond, &idoc, cx, now),
        Ok(idoc) => {
            let target = SetTarget { key, path, program: &program, cond, idoc: &idoc };
            set_path(reply, store, &target, cx, now)
        }
        Err(error) => reply.decline(error),
    };
    cx.node.json_ingest_buf.replace(buf);
    settled
}

/// A root set is a post-image (`json_set`) whose reply is known only after
/// it: the `Status` shape is reserved before the store call. Root TTL
/// semantics: preserved (the key is replaced, not recreated).
fn set_root(
    reply: JsonReply<'_, '_>,
    store: &mut CellStore,
    key: &[u8],
    cond: SetCond,
    idoc: &CanonicalDoc<'_>,
    cx: &ConnCx,
    now: Nanos,
) -> Settled {
    let fixed = match reply.reserve_fixed(FixedShape::Status) {
        FixedReservation::Reserved(fixed) => fixed,
        FixedReservation::Refused(settled) => return settled,
    };
    if let Err(error) = durable_full_fits(cx, key, idoc.as_bytes()) {
        return fixed.decline(error);
    }
    let opts = JsonSetOptions { cond, expire: SetExpire::Keep };
    match store.json_set(key, idoc, opts, now) {
        Ok(JsonSetOutcome::Applied) => {
            capture_full(cx);
            fixed.write(FixedValue::Ok)
        }
        Ok(JsonSetOutcome::Skipped) => fixed.write(FixedValue::Null),
        Err(error) => fixed.decline(ReplyError::Op(error)),
    }
}

/// A path `JSON.SET` after parsing: its key, path text, program, condition
/// and parsed value.
struct SetTarget<'t> {
    key: &'t [u8],
    path: &'t [u8],
    program: &'t PathProgram,
    cond: SetCond,
    idoc: &'t CanonicalDoc<'t>,
}

/// Path sets run replace-or-create per ADR-0041 D6.
fn set_path(
    reply: JsonReply<'_, '_>,
    store: &mut CellStore,
    target: &SetTarget<'_>,
    cx: &ConnCx,
    now: Nanos,
) -> Settled {
    let frozen = match frozen_doc(store, target.key, now, CREATE_AT_ROOT) {
        Ok(frozen) => frozen,
        Err(error) => return reply.decline(error),
    };
    let doc = TapeDoc::from_validated_bytes(&frozen);
    let limits = eval_limits(store);
    let matches = match eval(target.program, DocValue::from(doc.root()), &limits) {
        Ok(matches) => matches,
        Err(error) => return reply.decline(ReplyError::Eval(error)),
    };
    let skipped = match target.cond {
        SetCond::IfAbsent => !matches.is_empty(),
        SetCond::IfPresent => matches.is_empty(),
        SetCond::Always => false,
    };
    if skipped {
        return answer(reply, JsonReply::null);
    }
    let fragment = target.idoc.body();
    if !matches.is_empty() {
        let op = ApplyOp::SetReplace { fragment };
        return set_apply(reply, store, target, &doc, target.program, &op, cx, now);
    }
    // Creation: only a plain final child name creates, on every matched
    // parent object (ADR-0041 D6).
    let ast = inf_doc::path::parse_ast(target.path).expect("compile above accepted this text");
    let Some(Segment::Child(name)) = ast.segments.last() else {
        return reply.decline(ReplyError::PathMissing(target.path));
    };
    let parent = inf_doc::path::encode_ast(&inf_doc::path::PathAst {
        legacy: ast.legacy,
        segments: ast.segments[..ast.segments.len() - 1].to_vec(),
    })
    .expect("a prefix of an accepted program encodes under the ceiling");
    let name = name.clone();
    let op = ApplyOp::SetMember { key: &name, fragment };
    set_apply(reply, store, target, &doc, &parent, &op, cx, now)
}

#[allow(clippy::too_many_arguments)]
fn set_apply(
    reply: JsonReply<'_, '_>,
    store: &mut CellStore,
    target: &SetTarget<'_>,
    doc: &TapeDoc<'_>,
    program: &PathProgram,
    op: &ApplyOp<'_>,
    cx: &ConnCx,
    now: Nanos,
) -> Settled {
    match apply(doc, program, op, &eval_limits(store), store.doc_limits()) {
        Ok(outcome) if outcome.document.is_some() => {
            let edit = PathEdit { key: target.key, program, op, outcome: &outcome };
            commit_delta(reply, &edit, store, cx, now, |reply| Ok(reply.ok()?))
        }
        // No eligible site (every parent skipped): the frozen
        // path-does-not-exist arm.
        Ok(_) => reply.decline(ReplyError::PathMissing(target.path)),
        Err(error) => reply.decline(ReplyError::Apply(error)),
    }
}

// ---- JSON.GET / JSON.MGET ---------------------------------------------------

/// Parsed `JSON.GET` argument tail: formatting options + path list.
struct GetArgs<'a> {
    opts: SerializeOpts<'a>,
    paths: Vec<&'a [u8]>,
}

fn parse_get_args<'a>(argv: &'a (impl Argv + ?Sized)) -> Result<GetArgs<'a>, ReplyError<'a>> {
    let mut opts = SerializeOpts::default();
    let mut paths: Vec<&[u8]> = Vec::new();
    let mut i = 2;
    while i < argv.len() {
        let arg = argv.arg(i);
        let takes_value = arg.eq_ignore_ascii_case(b"INDENT")
            || arg.eq_ignore_ascii_case(b"NEWLINE")
            || arg.eq_ignore_ascii_case(b"SPACE");
        if takes_value {
            let Some(value) = (i + 1 < argv.len()).then(|| argv.arg(i + 1)) else {
                return Err(ReplyError::Line("ERR syntax error"));
            };
            if arg.eq_ignore_ascii_case(b"INDENT") {
                opts.indent = value;
            } else if arg.eq_ignore_ascii_case(b"NEWLINE") {
                opts.newline = value;
            } else {
                opts.space = value;
            }
            i += 2;
        } else if arg.eq_ignore_ascii_case(b"NOESCAPE") {
            i += 1; // Accepted and ignored (RedisJSON legacy no-op).
        } else {
            paths.push(arg);
            i += 1;
        }
    }
    if paths.is_empty() {
        paths.push(b".");
    }
    Ok(GetArgs { opts, paths })
}

fn get(
    argv: &(impl Argv + ?Sized),
    store: &mut CellStore,
    cx: &ConnCx,
    now: Nanos,
    mut reply: JsonReply<'_, '_>,
) -> Settled {
    let built = get_build(argv, store, cx, now, &mut reply);
    reply.settle(built)
}

fn get_build<'a>(
    argv: &'a (impl Argv + ?Sized),
    store: &mut CellStore,
    cx: &ConnCx,
    now: Nanos,
    reply: &mut JsonReply<'_, '_>,
) -> ReplyBuild<'a> {
    let args = parse_get_args(argv)?;
    // Compile every path before touching the store (cheap misses beat a
    // half-evaluated command), collecting owned programs.
    let mut programs = Vec::with_capacity(args.paths.len());
    for path in &args.paths {
        programs.push(compile(store, cx, path)?);
    }
    let limits = eval_limits(store);
    let Some(read) = read_doc(store, argv.arg(1), now)? else {
        return Ok(reply.null()?);
    };
    let mut match_sets: Vec<Matches> = Vec::with_capacity(programs.len());
    for (program, path) in programs.iter().zip(&args.paths) {
        let matches = eval(program, read.root, &limits).map_err(ReplyError::Eval)?;
        if program.is_legacy() && matches.is_empty() {
            return Err(ReplyError::PathMissing(path).into());
        }
        match_sets.push(matches);
    }
    let tree = if programs.len() == 1 {
        path_reply(read.root, &programs[0], &match_sets[0])
    } else {
        let members = args
            .paths
            .iter()
            .zip(programs.iter().zip(&match_sets))
            .map(|(path, (program, m))| (*path, path_reply(read.root, program, m)))
            .collect();
        Reply::Object(members)
    };
    Ok(reply.reply_tree(&tree, &args.opts)?)
}

/// One path's reply subtree: `$` mode wraps every match in an array;
/// legacy answers the first match (reads — ADR-0041 D7; the zero-match
/// legacy error was handled at eval time).
fn path_reply<'a>(root: DocValue<'a>, program: &PathProgram, matches: &Matches) -> Reply<'a> {
    let resolve_match =
        |i: usize| resolve(root, matches.get(i)).expect("matches resolve on their own document");
    if program.is_legacy() {
        debug_assert!(!matches.is_empty(), "legacy zero-match errored at eval");
        return Reply::Value(resolve_match(0));
    }
    Reply::Array((0..matches.len()).map(|i| Reply::Value(resolve_match(i))).collect())
}

fn mget(
    argv: &(impl Argv + ?Sized),
    store: &mut CellStore,
    cx: &ConnCx,
    now: Nanos,
    reply: JsonReply<'_, '_>,
) -> Settled {
    let path = argv.arg(argv.len() - 1);
    let program = match compile(store, cx, path) {
        Ok(program) => program,
        Err(error) => return reply.decline(error),
    };
    let limits = eval_limits(store);
    let mut elements = reply.per_element_array(argv.len() - 2);
    for i in 1..argv.len() - 1 {
        // Per-key element: missing and non-document keys answer nil
        // (RedisJSON MGET semantics); legacy paths answer the first
        // match, `$` paths the full match array ("[]" when none).
        let element = match store.json_get(argv.arg(i), now) {
            Ok(Some(read)) => match eval(&program, read.root, &limits) {
                Ok(m) if program.is_legacy() && m.is_empty() => None,
                Ok(m) => Some(path_reply(read.root, &program, &m)),
                Err(_) => None,
            },
            Ok(None) | Err(_) => None,
        };
        match element {
            Some(tree) => elements.document(&tree),
            None => elements.null(),
        }
    }
    elements.finish()
}

// ---- JSON.DEL / JSON.FORGET / JSON.TYPE --------------------------------------

fn del(
    argv: &(impl Argv + ?Sized),
    store: &mut CellStore,
    cx: &ConnCx,
    now: Nanos,
    reply: JsonReply<'_, '_>,
) -> Settled {
    let key = argv.arg(1);
    let path: &[u8] = if argv.len() > 2 { argv.arg(2) } else { b"." };
    let program = match compile(store, cx, path) {
        Ok(program) => program,
        Err(error) => return reply.decline(error),
    };
    if program.is_root() {
        return del_root(reply, store, key, cx, now);
    }
    let frozen = match store.json_freeze(key, now) {
        Ok(Some(frozen)) => frozen,
        Ok(None) => return answer(reply, |reply| reply.int(0)),
        Err(error) => return reply.decline(ReplyError::Op(error)),
    };
    let doc = TapeDoc::from_validated_bytes(&frozen);
    let op = ApplyOp::Del;
    match apply(&doc, &program, &op, &eval_limits(store), store.doc_limits()) {
        Ok(outcome) => {
            let applied = i64::from(outcome.applied);
            let edit = PathEdit { key, program: &program, op: &op, outcome: &outcome };
            commit_delta(reply, &edit, store, cx, now, |reply| Ok(reply.int(applied)?))
        }
        Err(error) => reply.decline(ReplyError::Apply(error)),
    }
}

/// Root deletion is key-level lifecycle (kernel-owned Delete record at
/// S17), never a path edit — after the type gate. Its count is known only
/// after the delete, so the `Count` shape is reserved first.
fn del_root(
    reply: JsonReply<'_, '_>,
    store: &mut CellStore,
    key: &[u8],
    cx: &ConnCx,
    now: Nanos,
) -> Settled {
    match store.type_of(key, now) {
        Some(inf_store::TypeTag::JsonDoc) => {}
        Some(_) => return reply.decline(ReplyError::Op(OpError::WrongType)),
        None => return answer(reply, |reply| reply.int(0)),
    }
    // The planted canary deletes before it reserves; the reply oracle must
    // see a refused root delete whose key is gone.
    let early = cfg!(inf_canary_json_fixed_unreserved).then(|| store.del(key, now));
    let fixed = match reply.reserve_fixed(FixedShape::Count) {
        FixedReservation::Reserved(fixed) => fixed,
        FixedReservation::Refused(settled) => return settled,
    };
    let deleted = match early {
        Some(deleted) => deleted,
        None => store.del(key, now),
    };
    if deleted {
        capture_delete(cx);
    }
    fixed.write(FixedValue::Deleted(deleted))
}

fn type_of(
    argv: &(impl Argv + ?Sized),
    store: &mut CellStore,
    cx: &ConnCx,
    now: Nanos,
    mut reply: JsonReply<'_, '_>,
) -> Settled {
    let built = type_of_build(argv, store, cx, now, &mut reply);
    reply.settle(built)
}

fn type_of_build<'a>(
    argv: &'a (impl Argv + ?Sized),
    store: &mut CellStore,
    cx: &ConnCx,
    now: Nanos,
    reply: &mut JsonReply<'_, '_>,
) -> ReplyBuild<'a> {
    let path: &[u8] = if argv.len() > 2 { argv.arg(2) } else { b"." };
    let program = compile(store, cx, path)?;
    let limits = eval_limits(store);
    let Some(read) = read_doc(store, argv.arg(1), now)? else {
        return Ok(reply.null()?);
    };
    let matches = eval(&program, read.root, &limits).map_err(ReplyError::Eval)?;
    let name = |i: usize| {
        type_name(resolve(read.root, matches.get(i)).expect("matches resolve on their document"))
    };
    match (reply.protocol(), program.is_legacy()) {
        (Protocol::Resp2, true) => match matches.is_empty() {
            true => reply.null()?,
            false => reply.bulk(name(0).as_bytes())?,
        },
        (Protocol::Resp2, false) => {
            reply.array_header(matches.len())?;
            for i in 0..matches.len() {
                reply.bulk(name(i).as_bytes())?;
            }
        }
        (Protocol::Resp3, true) => {
            reply.array_header(1)?;
            match matches.is_empty() {
                true => reply.null()?,
                false => reply.bulk(name(0).as_bytes())?,
            }
        }
        (Protocol::Resp3, false) => {
            reply.array_header(matches.len())?;
            for i in 0..matches.len() {
                reply.array_header(1)?;
                reply.bulk(name(i).as_bytes())?;
            }
        }
    }
    Ok(())
}

fn type_name(value: DocValue<'_>) -> &'static str {
    match value {
        DocValue::Null => "null",
        DocValue::Bool(_) => "boolean",
        DocValue::I64(_) => "integer",
        DocValue::F64(_) => "number",
        DocValue::Str(_) => "string",
        DocValue::Obj(_) => "object",
        DocValue::Arr(_) => "array",
    }
}

// ---- scalar mutations (M3-S12) ----------------------------------------------

/// `NUMINCRBY`/`NUMMULTBY`: the `Number` shape is reserved before the
/// in-place probe, which is the effect; `Unsupported` hands the account
/// back with the probe's proof, and the general path then carries the
/// fixed floor (ADR-0099 A1, "Fixed, then charged").
#[allow(clippy::wildcard_enum_match_arm, reason = "ADR-0143: column handler")]
fn num_op(
    id: CommandId,
    argv: &(impl Argv + ?Sized),
    store: &mut CellStore,
    cx: &ConnCx,
    now: Nanos,
    reply: JsonReply<'_, '_>,
) -> Settled {
    let (key, path) = (argv.arg(1), argv.arg(2));
    let Some(operand) = parse_number_operand(argv.arg(3)) else {
        return reply.decline(ReplyError::Line("ERR value is not a number"));
    };
    let op = match id {
        CommandId::JsonNumIncrBy => ApplyOp::NumIncrBy(operand),
        _ => ApplyOp::NumMultBy(operand),
    };
    let program = match compile(store, cx, path) {
        Ok(program) => program,
        Err(error) => return reply.decline(error),
    };
    let fixed = match reply.reserve_fixed(FixedShape::Number) {
        FixedReservation::Reserved(fixed) => fixed,
        FixedReservation::Refused(settled) => return settled,
    };
    let legacy = program.is_legacy();
    let unapplied = match store.json_patch_scalar(key, &program, &op, now) {
        Ok(Some(JsonScalarPatch::Number(number))) => {
            capture_delta(cx, &program, &op, NonZeroU32::MIN);
            return fixed.write(FixedValue::Number { legacy, number });
        }
        Ok(Some(JsonScalarPatch::Missing)) if legacy => {
            return fixed.decline(ReplyError::PathMissing(path));
        }
        Ok(Some(JsonScalarPatch::Missing)) => return fixed.write(FixedValue::NumberMissing),
        Ok(Some(JsonScalarPatch::Skipped)) if legacy => {
            return fixed.decline(ReplyError::NotContaining { path, noun: "a number" });
        }
        Ok(Some(JsonScalarPatch::Skipped)) => return fixed.write(FixedValue::NumberSkipped),
        Ok(Some(JsonScalarPatch::Unsupported(unapplied))) => unapplied,
        Ok(Some(JsonScalarPatch::Toggled(_))) => unreachable!("numeric probe returns a number"),
        Ok(None) => return fixed.decline(ReplyError::Line(MISSING_KEY)),
        Err(OpError::Overflow) => return fixed.decline(ReplyError::Apply(ApplyError::Overflow)),
        Err(OpError::NanOrInf) => return fixed.decline(ReplyError::Apply(ApplyError::NotANumber)),
        Err(error) => return fixed.decline(ReplyError::Op(error)),
    };
    let reply = fixed.reopen(unapplied);
    let outcome = match mutate_with(store, key, &program, &op, now) {
        Ok(outcome) => outcome,
        Err(error) => return reply.decline(error),
    };
    let edit = PathEdit { key, program: &program, op: &op, outcome: &outcome };
    commit_delta(reply, &edit, store, cx, now, |reply| {
        number_reply(reply, path, legacy, &outcome.results)
    })
}

/// The general path's numeric reply: legacy answers the last applied
/// match (RESP3 wraps it in a one-element array); `$` mode answers every
/// match — RESP3 a native array, RESP2 one bulk of JSON text.
fn number_reply<'a>(
    reply: &mut JsonReply<'_, '_>,
    path: &'a [u8],
    legacy: bool,
    results: &[MatchResult],
) -> ReplyBuild<'a> {
    if legacy {
        return match last_applied(results) {
            Some(MatchResult::Num(number)) => {
                if reply.protocol() == Protocol::Resp3 {
                    reply.array_header(1)?;
                }
                Ok(reply.number(number)?)
            }
            Some(_) => unreachable!("numeric ops apply numbers"),
            None if results.is_empty() => Err(ReplyError::PathMissing(path).into()),
            None => Err(ReplyError::NotContaining { path, noun: "a number" }.into()),
        };
    }
    if reply.protocol() == Protocol::Resp3 {
        reply.array_header(results.len())?;
        for result in results {
            match number_of(result) {
                Some(number) => reply.number(number)?,
                None => reply.null()?,
            }
        }
        return Ok(());
    }
    let members = results
        .iter()
        .map(|result| match number_of(result) {
            Some(Number::I64(value)) => Reply::Value(DocValue::I64(value)),
            Some(Number::F64(value)) => Reply::Value(DocValue::F64(value)),
            None => Reply::Value(DocValue::Null),
        })
        .collect();
    Ok(reply.reply_tree(&Reply::Array(members), &SerializeOpts::default())?)
}

fn number_of(result: &MatchResult) -> Option<Number> {
    match result {
        MatchResult::Num(number) => Some(*number),
        MatchResult::Skipped
        | MatchResult::Len(_)
        | MatchResult::Toggled(_)
        | MatchResult::Cleared
        | MatchResult::Removed
        | MatchResult::Set
        | MatchResult::Popped(_)
        | MatchResult::PoppedEmpty => None,
    }
}

/// Operand of NUMINCRBY/NUMMULTBY: a standalone JSON number token parsed
/// by the ingest parser's shared grammar without constructing an idoc.
fn parse_number_operand(text: &[u8]) -> Option<Number> {
    inf_doc::parse_number_token(text).ok()
}

fn str_append(
    argv: &(impl Argv + ?Sized),
    store: &mut CellStore,
    cx: &ConnCx,
    now: Nanos,
    reply: JsonReply<'_, '_>,
) -> Settled {
    let key = argv.arg(1);
    let (path, value): (&[u8], &[u8]) = if argv.len() == 3 {
        (b".", argv.arg(2)) // The RedisJSON-compatible implicit-root quirk.
    } else {
        (argv.arg(2), argv.arg(3))
    };
    // Default limits on purpose: the operand is one wire-bounded scalar;
    // the namespace's size limit binds the *result* below (`document too
    // large`), and parsing the operand under it would turn that refusal
    // into `value is not a string`.
    let mut parser = inf_doc::JsonParser::new();
    let Ok(operand_doc) = parser.parse(value) else {
        return reply.decline(ReplyError::Line("ERR value is not a string"));
    };
    let operand = TapeDoc::from_validated_bytes(&operand_doc);
    let DocValue::Str(payload) = DocValue::from(operand.root()) else {
        return reply.decline(ReplyError::Line("ERR value is not a string"));
    };
    let op = ApplyOp::StrAppend(payload.as_bytes());
    let count = PerMatchCount { noun: "a string", project: string_len_result };
    mutate_counting(reply, store, cx, key, path, &op, now, count)
}

fn string_len_result(result: &MatchResult) -> Option<i64> {
    match result {
        MatchResult::Len(n) => Some(*n as i64),
        MatchResult::Skipped
        | MatchResult::Num(_)
        | MatchResult::Toggled(_)
        | MatchResult::Cleared
        | MatchResult::Removed
        | MatchResult::Set
        | MatchResult::Popped(_)
        | MatchResult::PoppedEmpty => None,
    }
}

fn str_len(
    argv: &(impl Argv + ?Sized),
    store: &mut CellStore,
    cx: &ConnCx,
    now: Nanos,
    reply: JsonReply<'_, '_>,
) -> Settled {
    int_read(argv, store, cx, now, reply, "a string", |v| match v {
        DocValue::Str(s) => Some(s.as_bytes().len() as i64),
        DocValue::Null
        | DocValue::Bool(_)
        | DocValue::I64(_)
        | DocValue::F64(_)
        | DocValue::Obj(_)
        | DocValue::Arr(_) => None,
    })
}

/// The shared read-only per-match integer skeleton (`STRLEN`/`ARRLEN`/
/// `OBJLEN`): optional path defaults to legacy root; missing key answers
/// null; `$` mode arrays with nulls for inapplicable matches; legacy
/// answers the first match or the pinned `does not contain a {noun}`
/// error. (The legacy zero-match arm is checked **before** projecting —
/// the S13 review caught the previous tuple-match shape evaluating
/// `matches.get(0)` on an empty set, a reachable panic.)
fn int_read(
    argv: &(impl Argv + ?Sized),
    store: &mut CellStore,
    cx: &ConnCx,
    now: Nanos,
    mut reply: JsonReply<'_, '_>,
    noun: &'static str,
    project: impl Fn(DocValue<'_>) -> Option<i64>,
) -> Settled {
    let path: &[u8] = if argv.len() > 2 { argv.arg(2) } else { b"." };
    let built = int_read_build(argv, store, cx, now, &mut reply, (path, noun), project);
    reply.settle(built)
}

fn int_read_build<'a>(
    argv: &(impl Argv + ?Sized),
    store: &mut CellStore,
    cx: &ConnCx,
    now: Nanos,
    reply: &mut JsonReply<'_, '_>,
    (path, noun): (&'a [u8], &'static str),
    project: impl Fn(DocValue<'_>) -> Option<i64>,
) -> ReplyBuild<'a> {
    let program = compile(store, cx, path)?;
    let limits = eval_limits(store);
    let Some(read) = read_doc(store, argv.arg(1), now)? else {
        return Ok(reply.null()?);
    };
    let matches = eval(&program, read.root, &limits).map_err(ReplyError::Eval)?;
    let value_of = |i: usize| resolve(read.root, matches.get(i)).and_then(&project);
    if program.is_legacy() {
        if matches.is_empty() {
            return Err(ReplyError::PathMissing(path).into());
        }
        return match value_of(0) {
            Some(n) => Ok(reply.int(n)?),
            None => Err(ReplyError::NotContaining { path, noun }.into()),
        };
    }
    reply.array_header(matches.len())?;
    for i in 0..matches.len() {
        match value_of(i) {
            Some(n) => reply.int(n)?,
            None => reply.null()?,
        }
    }
    Ok(())
}

/// `TOGGLE`: the `Toggle` shape is reserved before the in-place probe;
/// `Unsupported` reopens the account for the general path, which then
/// carries the fixed floor (ADR-0099 A1, "Fixed, then charged").
fn toggle(
    argv: &(impl Argv + ?Sized),
    store: &mut CellStore,
    cx: &ConnCx,
    now: Nanos,
    reply: JsonReply<'_, '_>,
) -> Settled {
    let key = argv.arg(1);
    let path: &[u8] = if argv.len() > 2 { argv.arg(2) } else { b"." };
    let op = ApplyOp::Toggle;
    let program = match compile(store, cx, path) {
        Ok(program) => program,
        Err(error) => return reply.decline(error),
    };
    let fixed = match reply.reserve_fixed(FixedShape::Toggle) {
        FixedReservation::Reserved(fixed) => fixed,
        FixedReservation::Refused(settled) => return settled,
    };
    let legacy = program.is_legacy();
    let unapplied = match store.json_patch_scalar(key, &program, &op, now) {
        Ok(Some(JsonScalarPatch::Toggled(value))) => {
            capture_delta(cx, &program, &op, NonZeroU32::MIN);
            return fixed.write(FixedValue::Toggled { legacy, value });
        }
        Ok(Some(JsonScalarPatch::Missing)) if legacy => {
            return fixed.decline(ReplyError::PathMissing(path));
        }
        Ok(Some(JsonScalarPatch::Missing)) => return fixed.write(FixedValue::ToggleMissing),
        Ok(Some(JsonScalarPatch::Skipped)) if legacy => {
            return fixed.decline(ReplyError::NotContaining { path, noun: "a boolean" });
        }
        Ok(Some(JsonScalarPatch::Skipped)) => return fixed.write(FixedValue::ToggleSkipped),
        Ok(Some(JsonScalarPatch::Unsupported(unapplied))) => unapplied,
        Ok(Some(JsonScalarPatch::Number(_))) => unreachable!("toggle probe returns a boolean"),
        Ok(None) => return fixed.decline(ReplyError::Line(MISSING_KEY)),
        Err(error) => return fixed.decline(ReplyError::Op(error)),
    };
    let reply = fixed.reopen(unapplied);
    let outcome = match mutate_with(store, key, &program, &op, now) {
        Ok(outcome) => outcome,
        Err(error) => return reply.decline(error),
    };
    let edit = PathEdit { key, program: &program, op: &op, outcome: &outcome };
    commit_delta(reply, &edit, store, cx, now, |reply| {
        toggle_reply(reply, path, legacy, &outcome.results)
    })
}

fn toggle_reply<'a>(
    reply: &mut JsonReply<'_, '_>,
    path: &'a [u8],
    legacy: bool,
    results: &[MatchResult],
) -> ReplyBuild<'a> {
    if legacy {
        return match last_applied(results) {
            Some(MatchResult::Toggled(value)) => {
                Ok(reply.bulk(if value { b"true" } else { b"false" })?)
            }
            Some(_) => unreachable!("toggle applies booleans"),
            None if results.is_empty() => Err(ReplyError::PathMissing(path).into()),
            None => Err(ReplyError::NotContaining { path, noun: "a boolean" }.into()),
        };
    }
    reply.array_header(results.len())?;
    for result in results {
        match result {
            MatchResult::Toggled(value) => reply.int(i64::from(*value))?,
            MatchResult::Skipped
            | MatchResult::Num(_)
            | MatchResult::Len(_)
            | MatchResult::Cleared
            | MatchResult::Removed
            | MatchResult::Set
            | MatchResult::Popped(_)
            | MatchResult::PoppedEmpty => reply.null()?,
        }
    }
    Ok(())
}

fn clear(
    argv: &(impl Argv + ?Sized),
    store: &mut CellStore,
    cx: &ConnCx,
    now: Nanos,
    reply: JsonReply<'_, '_>,
) -> Settled {
    let key = argv.arg(1);
    let path: &[u8] = if argv.len() > 2 { argv.arg(2) } else { b"." };
    let op = ApplyOp::Clear;
    let (program, outcome) = match mutate(store, cx, key, path, &op, now) {
        Ok(planned) => planned,
        Err(error) => return reply.decline(error),
    };
    let applied = i64::from(outcome.applied);
    let edit = PathEdit { key, program: &program, op: &op, outcome: &outcome };
    commit_delta(reply, &edit, store, cx, now, |reply| Ok(reply.int(applied)?))
}

// ---- array ops (M3-S13, ADR-0042 D1–D4) ---------------------------------------

fn arr_append(
    argv: &(impl Argv + ?Sized),
    store: &mut CellStore,
    cx: &ConnCx,
    now: Nanos,
    reply: JsonReply<'_, '_>,
) -> Settled {
    let key = argv.arg(1);
    // The optional-path quirk (ADR-0042 D7): three arguments mean a
    // legacy root path and a single value — the STRAPPEND precedent.
    let (path, first_value): (&[u8], usize) =
        if argv.len() == 3 { (b".", 2) } else { (argv.arg(2), 3) };
    let operand = match parse_array_operand(store, cx, argv, first_value) {
        Ok(operand) => operand,
        Err(error) => return reply.decline(error),
    };
    let op = ApplyOp::ArrAppend { elements: &operand };
    mutate_counting(reply, store, cx, key, path, &op, now, ARRAY_LEN)
}

fn arr_insert(
    argv: &(impl Argv + ?Sized),
    store: &mut CellStore,
    cx: &ConnCx,
    now: Nanos,
    reply: JsonReply<'_, '_>,
) -> Settled {
    let (key, path) = (argv.arg(1), argv.arg(2));
    let Ok(index) = parse_i64(argv.arg(3)) else {
        return reply.decline(ReplyError::Line("ERR value is not an integer or out of range"));
    };
    let operand = match parse_array_operand(store, cx, argv, 4) {
        Ok(operand) => operand,
        Err(error) => return reply.decline(error),
    };
    let op = ApplyOp::ArrInsert { index, elements: &operand };
    mutate_counting(reply, store, cx, key, path, &op, now, ARRAY_LEN)
}

fn arr_trim(
    argv: &(impl Argv + ?Sized),
    store: &mut CellStore,
    cx: &ConnCx,
    now: Nanos,
    reply: JsonReply<'_, '_>,
) -> Settled {
    let (key, path) = (argv.arg(1), argv.arg(2));
    let (Ok(start), Ok(stop)) = (parse_i64(argv.arg(3)), parse_i64(argv.arg(4))) else {
        return reply.decline(ReplyError::Line("ERR value is not an integer or out of range"));
    };
    let op = ApplyOp::ArrTrim { start, stop };
    mutate_counting(reply, store, cx, key, path, &op, now, ARRAY_LEN)
}

const ARRAY_LEN: PerMatchCount = PerMatchCount { noun: "an array", project: array_len_result };

fn array_len_result(r: &MatchResult) -> Option<i64> {
    match r {
        MatchResult::Len(n) => Some(*n as i64),
        MatchResult::Skipped
        | MatchResult::Num(_)
        | MatchResult::Toggled(_)
        | MatchResult::Cleared
        | MatchResult::Removed
        | MatchResult::Set
        | MatchResult::Popped(_)
        | MatchResult::PoppedEmpty => None,
    }
}

fn arr_pop(
    argv: &(impl Argv + ?Sized),
    store: &mut CellStore,
    cx: &ConnCx,
    now: Nanos,
    reply: JsonReply<'_, '_>,
) -> Settled {
    let key = argv.arg(1);
    let path: &[u8] = if argv.len() > 2 { argv.arg(2) } else { b"." };
    let mut index = -1i64;
    if argv.len() > 3 {
        let Ok(parsed) = parse_i64(argv.arg(3)) else {
            return reply.decline(ReplyError::Line("ERR value is not an integer or out of range"));
        };
        index = parsed;
    }
    let program = match compile(store, cx, path) {
        Ok(program) => program,
        Err(error) => return reply.decline(error),
    };
    let frozen = match frozen_doc(store, key, now, MISSING_KEY) {
        Ok(frozen) => frozen,
        Err(error) => return reply.decline(error),
    };
    let doc = TapeDoc::from_validated_bytes(&frozen);
    let op = ApplyOp::ArrPop { index };
    let outcome = match apply(&doc, &program, &op, &eval_limits(store), store.doc_limits()) {
        Ok(outcome) => outcome,
        Err(error) => return reply.decline(ReplyError::Apply(error)),
    };
    let legacy = program.is_legacy();
    let edit = PathEdit { key, program: &program, op: &op, outcome: &outcome };
    // The popped elements serialize from the frozen pre-image (`Popped`
    // offsets are meaningful only against it — ADR-0042 D4), inside
    // `commit_delta` and so before the pop commits (ADR-0099 A1).
    commit_delta(reply, &edit, store, cx, now, |reply| {
        pop_reply(reply, &doc, path, legacy, &outcome.results)
    })
}

fn pop_reply<'a>(
    reply: &mut JsonReply<'_, '_>,
    doc: &TapeDoc<'_>,
    path: &'a [u8],
    legacy: bool,
    results: &[MatchResult],
) -> ReplyBuild<'a> {
    let opts = SerializeOpts::default();
    let popped = |at: u32| DocValue::from(doc.value_at(at as usize));
    if legacy {
        // Last array match wins: its popped value, or null when it was
        // empty; no array match at all takes the type/path error arms.
        let last_array = results
            .iter()
            .rev()
            .find(|r| matches!(r, MatchResult::Popped(_) | MatchResult::PoppedEmpty));
        return match last_array {
            Some(MatchResult::Popped(at)) => Ok(reply.document(popped(*at), &opts)?),
            Some(_) => Ok(reply.null()?),
            None if results.is_empty() => Err(ReplyError::PathMissing(path).into()),
            None => Err(ReplyError::NotContaining { path, noun: "an array" }.into()),
        };
    }
    reply.array_header(results.len())?;
    for result in results {
        match result {
            MatchResult::Popped(at) => reply.document(popped(*at), &opts)?,
            MatchResult::Skipped
            | MatchResult::Num(_)
            | MatchResult::Len(_)
            | MatchResult::Toggled(_)
            | MatchResult::Cleared
            | MatchResult::Removed
            | MatchResult::Set
            | MatchResult::PoppedEmpty => reply.null()?,
        }
    }
    Ok(())
}

fn arr_len(
    argv: &(impl Argv + ?Sized),
    store: &mut CellStore,
    cx: &ConnCx,
    now: Nanos,
    reply: JsonReply<'_, '_>,
) -> Settled {
    int_read(argv, store, cx, now, reply, "an array", |v| match v {
        DocValue::Arr(a) => Some(a.len() as i64),
        DocValue::Null
        | DocValue::Bool(_)
        | DocValue::I64(_)
        | DocValue::F64(_)
        | DocValue::Str(_)
        | DocValue::Obj(_) => None,
    })
}

/// The `ARRINDEX` needle: a scalar JSON value (ADR-0042 D3 — container
/// needles are rejected; number equality is numeric).
enum Needle {
    Null,
    Bool(bool),
    I64(i64),
    F64(f64),
    Str(Vec<u8>),
}

/// Default limits on purpose: a scalar needle is wire-bounded and never
/// stored.
fn parse_scalar_needle(text: &[u8]) -> Option<Needle> {
    let mut parser = inf_doc::JsonParser::new();
    let idoc = parser.parse(text).ok()?;
    let doc = TapeDoc::from_validated_bytes(&idoc);
    match DocValue::from(doc.root()) {
        DocValue::Null => Some(Needle::Null),
        DocValue::Bool(b) => Some(Needle::Bool(b)),
        DocValue::I64(v) => Some(Needle::I64(v)),
        DocValue::F64(v) => Some(Needle::F64(v)),
        DocValue::Str(s) => Some(Needle::Str(s.as_bytes().to_vec())),
        DocValue::Obj(_) | DocValue::Arr(_) => None,
    }
}

fn scalar_eq(value: DocValue<'_>, needle: &Needle) -> bool {
    match (value, needle) {
        (DocValue::Null, Needle::Null) => true,
        (DocValue::Bool(a), Needle::Bool(b)) => a == *b,
        (DocValue::I64(a), Needle::I64(b)) => a == *b,
        // Mixed-width numbers compare numerically (`1` matches `1.0`)
        // (ADR-0042 D3).
        (DocValue::I64(a), Needle::F64(b)) => a as f64 == *b,
        (DocValue::F64(a), Needle::I64(b)) => a == *b as f64,
        (DocValue::F64(a), Needle::F64(b)) => a == *b,
        (DocValue::Str(s), Needle::Str(b)) => s.as_bytes() == &b[..],
        _ => false,
    }
}

fn arr_index(
    argv: &(impl Argv + ?Sized),
    store: &mut CellStore,
    cx: &ConnCx,
    now: Nanos,
    mut reply: JsonReply<'_, '_>,
) -> Settled {
    let Some(needle) = parse_scalar_needle(argv.arg(3)) else {
        return reply.decline(ReplyError::Line("ERR value is not a scalar"));
    };
    let mut range = [0i64, 0i64];
    for (slot, i) in range.iter_mut().zip(4..argv.len()) {
        let Ok(parsed) = parse_i64(argv.arg(i)) else {
            return reply.decline(ReplyError::Line("ERR value is not an integer or out of range"));
        };
        *slot = parsed;
    }
    let search = ArraySearch { needle: &needle, start: range[0], stop: range[1] };
    let built = arr_index_build(argv, store, cx, now, &mut reply, &search);
    reply.settle(built)
}

/// One `ARRINDEX` query: the needle and its `[start, stop)` window.
struct ArraySearch<'n> {
    needle: &'n Needle,
    start: i64,
    stop: i64,
}

fn arr_index_build<'a>(
    argv: &'a (impl Argv + ?Sized),
    store: &mut CellStore,
    cx: &ConnCx,
    now: Nanos,
    reply: &mut JsonReply<'_, '_>,
    search: &ArraySearch<'_>,
) -> ReplyBuild<'a> {
    let (key, path) = (argv.arg(1), argv.arg(2));
    let program = compile(store, cx, path)?;
    let limits = eval_limits(store);
    let Some(read) = read_doc(store, key, now)? else {
        return Ok(reply.null()?);
    };
    let matches = eval(&program, read.root, &limits).map_err(ReplyError::Eval)?;
    let index_of = |i: usize| match resolve(read.root, matches.get(i)) {
        Some(DocValue::Arr(a)) => Some(array_search(&a, search.needle, search.start, search.stop)),
        _ => None,
    };
    if program.is_legacy() {
        if matches.is_empty() {
            return Err(ReplyError::PathMissing(path).into());
        }
        return match index_of(0) {
            Some(n) => Ok(reply.int(n)?),
            None => Err(ReplyError::NotContaining { path, noun: "an array" }.into()),
        };
    }
    reply.array_header(matches.len())?;
    for i in 0..matches.len() {
        match index_of(i) {
            Some(n) => reply.int(n)?,
            None => reply.null()?,
        }
    }
    Ok(())
}

/// First element in `[start, stop)` equal to the needle, or −1. `stop ==
/// 0` means end-of-array; negatives resolve from the end; both clamp
/// (ADR-0042 D3).
fn array_search(array: &inf_doc::ArrCursor<'_>, needle: &Needle, start: i64, stop: i64) -> i64 {
    let len = array.len() as i64;
    let resolve_end = |i: i64| if i < 0 { i + len } else { i };
    let first = resolve_end(start).clamp(0, len);
    let last = if stop == 0 { len } else { resolve_end(stop).clamp(0, len) };
    for (ordinal, element) in array.iter().enumerate() {
        let at = ordinal as i64;
        if at < first {
            continue;
        }
        if at >= last {
            break;
        }
        if scalar_eq(element, needle) {
            return at;
        }
    }
    -1
}

// ---- object ops + MERGE (M3-S14, ADR-0042 D5/D6) ------------------------------

fn obj_len(
    argv: &(impl Argv + ?Sized),
    store: &mut CellStore,
    cx: &ConnCx,
    now: Nanos,
    reply: JsonReply<'_, '_>,
) -> Settled {
    int_read(argv, store, cx, now, reply, "an object", |v| match v {
        DocValue::Obj(o) => Some(o.len() as i64),
        DocValue::Null
        | DocValue::Bool(_)
        | DocValue::I64(_)
        | DocValue::F64(_)
        | DocValue::Str(_)
        | DocValue::Arr(_) => None,
    })
}

fn obj_keys(
    argv: &(impl Argv + ?Sized),
    store: &mut CellStore,
    cx: &ConnCx,
    now: Nanos,
    mut reply: JsonReply<'_, '_>,
) -> Settled {
    let built = obj_keys_build(argv, store, cx, now, &mut reply);
    reply.settle(built)
}

/// Every key of every match, each charged as it is written (ADR-0099 A1):
/// duplicate union members repeat a match up to `max_matches` times.
fn obj_keys_build<'a>(
    argv: &'a (impl Argv + ?Sized),
    store: &mut CellStore,
    cx: &ConnCx,
    now: Nanos,
    reply: &mut JsonReply<'_, '_>,
) -> ReplyBuild<'a> {
    let path: &[u8] = if argv.len() > 2 { argv.arg(2) } else { b"." };
    let program = compile(store, cx, path)?;
    let limits = eval_limits(store);
    let Some(read) = read_doc(store, argv.arg(1), now)? else {
        return Ok(reply.null()?);
    };
    let matches = eval(&program, read.root, &limits).map_err(ReplyError::Eval)?;
    let object_of = |i: usize| match resolve(read.root, matches.get(i)) {
        Some(DocValue::Obj(o)) => Some(o),
        _ => None,
    };
    if program.is_legacy() {
        if matches.is_empty() {
            return Err(ReplyError::PathMissing(path).into());
        }
        return match object_of(0) {
            Some(o) => Ok(write_keys(&o, reply)?),
            None => Err(ReplyError::NotContaining { path, noun: "an object" }.into()),
        };
    }
    reply.array_header(matches.len())?;
    for i in 0..matches.len() {
        match object_of(i) {
            Some(o) => write_keys(&o, reply)?,
            None => reply.null()?,
        }
    }
    Ok(())
}

/// One object's keys, in insertion order — the only order the format has
/// (ADR-0036).
fn write_keys(object: &ObjCursor<'_>, reply: &mut JsonReply<'_, '_>) -> Result<(), ReplyTooLarge> {
    reply.array_header(object.len())?;
    for (key, _) in object.iter() {
        reply.bulk(key.as_bytes())?;
    }
    Ok(())
}

fn merge(
    argv: &(impl Argv + ?Sized),
    store: &mut CellStore,
    cx: &ConnCx,
    now: Nanos,
    reply: JsonReply<'_, '_>,
) -> Settled {
    let (key, path) = (argv.arg(1), argv.arg(2));
    let program = match compile(store, cx, path) {
        Ok(program) => program,
        Err(error) => return reply.decline(error),
    };
    let mut buf = cx.node.json_ingest_buf.take();
    let settled = match parse_value(store, cx, argv.arg(3), &mut buf) {
        Ok(idoc) => merge_parsed(reply, store, key, path, &program, &idoc, cx, now),
        Err(error) => reply.decline(error),
    };
    cx.node.json_ingest_buf.replace(buf);
    settled
}

/// The post-parse half of `JSON.MERGE` (ADR-0042 D6): existing matches
/// merge in place; a missing key creates at the root only; an existing
/// key with no matches follows the SET parent-creation rule with the
/// null-stripped patch.
#[allow(clippy::too_many_arguments)]
fn merge_parsed(
    reply: JsonReply<'_, '_>,
    store: &mut CellStore,
    key: &[u8],
    path: &[u8],
    program: &PathProgram,
    idoc: &CanonicalDoc<'_>,
    cx: &ConnCx,
    now: Nanos,
) -> Settled {
    let fragment = idoc.body();
    if program.is_root() && store.json_get(key, now).ok().flatten().is_none() {
        // Missing key, root path: create with MergePatch(absent, patch).
        // Wrong types surface through json_set's guard.
        return merge_create(reply, store, key, fragment, cx, now);
    }
    let frozen = match frozen_doc(store, key, now, CREATE_AT_ROOT) {
        Ok(frozen) => frozen,
        Err(error) => return reply.decline(error),
    };
    let doc = TapeDoc::from_validated_bytes(&frozen);
    let target = MergeTarget { key, path, doc: &doc };
    let matches = match eval(program, DocValue::from(doc.root()), &eval_limits(store)) {
        Ok(matches) => matches,
        Err(error) => return reply.decline(ReplyError::Eval(error)),
    };
    if !matches.is_empty() {
        return merge_apply(
            reply,
            store,
            &target,
            program,
            &ApplyOp::Merge { patch: fragment },
            cx,
            now,
        );
    }
    // The SET parent-creation rule (ADR-0041 D6), with the merged-against-
    // absent value (nulls stripped through object chains).
    let ast = inf_doc::path::parse_ast(path).expect("compile above accepted this text");
    let Some(Segment::Child(name)) = ast.segments.last() else {
        return reply.decline(ReplyError::PathMissing(path));
    };
    let parent = inf_doc::path::encode_ast(&inf_doc::path::PathAst {
        legacy: ast.legacy,
        segments: ast.segments[..ast.segments.len() - 1].to_vec(),
    })
    .expect("a prefix of an accepted program encodes under the ceiling");
    let name = name.clone();
    let created = inf_doc::merge_absent_document(fragment);
    let op = ApplyOp::SetMember { key: &name, fragment: created.body() };
    merge_apply(reply, store, &target, &parent, &op, cx, now)
}

/// `MERGE`'s root create is a post-image whose reply is known only after
/// the store call: the `Status` shape is reserved first.
fn merge_create(
    reply: JsonReply<'_, '_>,
    store: &mut CellStore,
    key: &[u8],
    fragment: &[u8],
    cx: &ConnCx,
    now: Nanos,
) -> Settled {
    let created = inf_doc::merge_absent_document(fragment);
    let fixed = match reply.reserve_fixed(FixedShape::Status) {
        FixedReservation::Reserved(fixed) => fixed,
        FixedReservation::Refused(settled) => return settled,
    };
    if let Err(error) = durable_full_fits(cx, key, created.as_bytes()) {
        return fixed.decline(error);
    }
    let opts = JsonSetOptions { cond: SetCond::Always, expire: SetExpire::Keep };
    match store.json_set(key, &created, opts, now) {
        Ok(JsonSetOutcome::Applied) => {
            capture_full(cx);
            fixed.write(FixedValue::Ok)
        }
        Ok(JsonSetOutcome::Skipped) => unreachable!("unconditional set applies"),
        Err(error) => fixed.decline(ReplyError::Op(error)),
    }
}

/// A path `JSON.MERGE` over an existing document.
struct MergeTarget<'t> {
    key: &'t [u8],
    path: &'t [u8],
    doc: &'t TapeDoc<'t>,
}

/// Run one merge-family apply + commit; a no-op merge (byte-equal
/// output, ADR-0041 D8) is still `+OK`.
fn merge_apply(
    reply: JsonReply<'_, '_>,
    store: &mut CellStore,
    target: &MergeTarget<'_>,
    program: &PathProgram,
    op: &ApplyOp<'_>,
    cx: &ConnCx,
    now: Nanos,
) -> Settled {
    match apply(target.doc, program, op, &eval_limits(store), store.doc_limits()) {
        Ok(outcome) => {
            if matches!(op, ApplyOp::SetMember { .. }) && outcome.document.is_none() {
                // Zero eligible parents: the oracle's path error arm.
                return reply.decline(ReplyError::PathMissing(target.path));
            }
            let edit = PathEdit { key: target.key, program, op, outcome: &outcome };
            commit_delta(reply, &edit, store, cx, now, |reply| Ok(reply.ok()?))
        }
        Err(error) => reply.decline(ReplyError::Apply(error)),
    }
}

/// Parse the trailing value arguments and wrap them as the single
/// ADR-0042 D2 canonical array operand. Its refusals — an element that
/// nests `DEPTH_MAX`, an operand past the size ceiling — answer through the
/// apply error texts, ahead of path compile and key lookup (ADR-0169 D3).
fn parse_array_operand(
    store: &CellStore,
    cx: &ConnCx,
    argv: &(impl Argv + ?Sized),
    first_value: usize,
) -> Result<Vec<u8>, ReplyError<'static>> {
    let mut docs: Vec<Vec<u8>> = Vec::with_capacity(argv.len() - first_value);
    for i in first_value..argv.len() {
        let mut out = Vec::new();
        parse_value(store, cx, argv.arg(i), &mut out)?;
        docs.push(out);
    }
    let fragments: Vec<&[u8]> = docs.iter().map(|d| &d[inf_doc::HEADER_LEN..]).collect();
    inf_doc::array_operand(&fragments).map_err(ReplyError::Apply)
}

/// The shared mutation prologue: compile, freeze, apply. Returns the
/// compiled program too — reply shaping branches on its recorded mode
/// (ADR-0040: mode lives on the program, never re-derived from text).
fn mutate(
    store: &mut CellStore,
    cx: &ConnCx,
    key: &[u8],
    path: &[u8],
    op: &ApplyOp<'_>,
    now: Nanos,
) -> Result<(PathProgram, ApplyOutcome), ReplyError<'static>> {
    let program = compile(store, cx, path)?;
    let outcome = mutate_with(store, key, &program, op, now)?;
    Ok((program, outcome))
}

/// Canonical fallback for callers that already compiled the program (the
/// scalar probe's `Unsupported` arm). One freeze/apply/error mapping path
/// keeps fast-path fallback behavior identical to ordinary mutations.
fn mutate_with(
    store: &mut CellStore,
    key: &[u8],
    program: &PathProgram,
    op: &ApplyOp<'_>,
    now: Nanos,
) -> Result<ApplyOutcome, ReplyError<'static>> {
    let frozen = frozen_doc(store, key, now, MISSING_KEY)?;
    let doc = TapeDoc::from_validated_bytes(&frozen);
    apply(&doc, program, op, &eval_limits(store), store.doc_limits()).map_err(ReplyError::Apply)
}

/// How a counting mutation (`STRAPPEND`, `ARRAPPEND`, `ARRINSERT`,
/// `ARRTRIM`) reads one match's result, and the noun of its type error.
#[derive(Copy, Clone)]
struct PerMatchCount {
    noun: &'static str,
    project: fn(&MatchResult) -> Option<i64>,
}

/// Plan a counting mutation and commit it with its per-match reply.
#[allow(clippy::too_many_arguments)]
fn mutate_counting(
    reply: JsonReply<'_, '_>,
    store: &mut CellStore,
    cx: &ConnCx,
    key: &[u8],
    path: &[u8],
    op: &ApplyOp<'_>,
    now: Nanos,
    count: PerMatchCount,
) -> Settled {
    let (program, outcome) = match mutate(store, cx, key, path, op, now) {
        Ok(planned) => planned,
        Err(error) => return reply.decline(error),
    };
    let legacy = program.is_legacy();
    let edit = PathEdit { key, program: &program, op, outcome: &outcome };
    commit_delta(reply, &edit, store, cx, now, |reply| {
        int_per_match(reply, path, legacy, &outcome.results, count)
    })
}

/// `$`-mode per-match integer array or the legacy last-match integer;
/// skipped matches answer nulls / the pinned `does not contain a {noun}`
/// type error.
fn int_per_match<'a>(
    reply: &mut JsonReply<'_, '_>,
    path: &'a [u8],
    legacy: bool,
    results: &[MatchResult],
    count: PerMatchCount,
) -> ReplyBuild<'a> {
    if legacy {
        return match last_applied(results).as_ref().and_then(count.project) {
            Some(n) => Ok(reply.int(n)?),
            None if results.is_empty() => Err(ReplyError::PathMissing(path).into()),
            None => Err(ReplyError::NotContaining { path, noun: count.noun }.into()),
        };
    }
    reply.array_header(results.len())?;
    for result in results {
        match (count.project)(result) {
            Some(n) => reply.int(n)?,
            None => reply.null()?,
        }
    }
    Ok(())
}

// ---- reply-shape matrix source (M3-S15, ADR-0042 D8) ---------------------------

/// One row of the generated `docs/json-reply-shapes.md` (§3.2: the
/// reply-shape matrix is a frozen, generated artifact — the compat
/// crate renders this table and its staleness test gates CI). The table
/// lives beside the handlers it describes so a shape change and its
/// declaration change in one diff. S21 byte-diffs this surface under both
/// protocols and publishes every accepted divergence in the compat matrix.
pub struct ReplyShape {
    pub name: &'static str,
    /// Carries `CmdFlags::WRITE` (the ADR-0041 D4 durable guard set).
    pub write: bool,
    /// Reply under a `$`-mode path.
    pub dollar: &'static str,
    /// Reply under a legacy path (first match for reads, last applied
    /// match for mutations — ADR-0041 D7).
    pub legacy: &'static str,
    /// RESP3 delta over RESP2, pinned to the RedisJSON oracle.
    pub resp3: &'static str,
    pub notes: &'static str,
}

const NULLS: &str = "nulls are `_` instead of `$-1`";

/// The declared shape matrix, one row per `JSON.*` registry command
/// (enforced 1:1 by the compat renderer).
pub static JSON_REPLY_SHAPES: &[ReplyShape] = &[
    ReplyShape {
        name: "JSON.SET",
        write: true,
        dollar: "`+OK`; null when NX/XX skips",
        legacy: "same as `$` mode",
        resp3: NULLS,
        notes: "parent-creation rules per ADR-0041 D6; root sets preserve TTL",
    },
    ReplyShape {
        name: "JSON.GET",
        write: false,
        dollar: "bulk JSON text: array of matches; multi-path wraps an object keyed by the \
                 path strings as given",
        legacy: "bulk JSON text: first match, unwrapped; zero matches error",
        resp3: NULLS,
        notes: "`INDENT`/`NEWLINE`/`SPACE` honored; missing key is null in both modes",
    },
    ReplyShape {
        name: "JSON.MGET",
        write: false,
        dollar: "array: per key, bulk JSON match-array or null",
        legacy: "array: per key, bulk first match or null",
        resp3: NULLS,
        notes: "per-key atomicity only — no cross-cell snapshot (ADR-0041 D9)",
    },
    ReplyShape {
        name: "JSON.DEL",
        write: true,
        dollar: "integer: matches removed",
        legacy: "integer: matches removed",
        resp3: "identical",
        notes: "root path deletes the key (kernel-owned lifecycle)",
    },
    ReplyShape {
        name: "JSON.FORGET",
        write: true,
        dollar: "integer: matches removed",
        legacy: "integer: matches removed",
        resp3: "identical",
        notes: "alias of JSON.DEL",
    },
    ReplyShape {
        name: "JSON.TYPE",
        write: false,
        dollar: "array of type-name bulk strings",
        legacy: "bulk string: first match's type; null when the path misses",
        resp3: "`$` mode: array of one-element bulk-string arrays; legacy: one-element array \
                containing the bulk string or null",
        notes: "`integer` and `number` are distinct names (RedisJSON parity)",
    },
    ReplyShape {
        name: "JSON.NUMINCRBY",
        write: true,
        dollar: "bulk JSON text array: new value per match, null for non-numbers",
        legacy: "bulk JSON text: last applied match's new value",
        resp3: "native integer/double/null array in both modes; legacy has one element",
        notes: "i64 overflow / non-finite results abort the whole command",
    },
    ReplyShape {
        name: "JSON.NUMMULTBY",
        write: true,
        dollar: "bulk JSON text array: new value per match, null for non-numbers",
        legacy: "bulk JSON text: last applied match's new value",
        resp3: "native integer/double/null array in both modes; legacy has one element",
        notes: "same numeric model as NUMINCRBY",
    },
    ReplyShape {
        name: "JSON.STRAPPEND",
        write: true,
        dollar: "array: new byte length per match, null for non-strings",
        legacy: "integer: last applied match's new length",
        resp3: NULLS,
        notes: "operand must be a JSON string; the no-path form appends at the legacy root",
    },
    ReplyShape {
        name: "JSON.STRLEN",
        write: false,
        dollar: "array: byte length per match, null for non-strings",
        legacy: "integer: first match's length",
        resp3: NULLS,
        notes: "missing key is null",
    },
    ReplyShape {
        name: "JSON.TOGGLE",
        write: true,
        dollar: "array: 0/1 per match, null for non-booleans",
        legacy: "bulk `true`/`false`: last applied match's new value",
        resp3: NULLS,
        notes: "booleans only; others skip",
    },
    ReplyShape {
        name: "JSON.CLEAR",
        write: true,
        dollar: "integer: values cleared",
        legacy: "integer: values cleared",
        resp3: "identical",
        notes: "already-empty containers and zero numbers skip, uncounted (ADR-0041 D8)",
    },
    ReplyShape {
        name: "JSON.ARRAPPEND",
        write: true,
        dollar: "array: new length per match, null for non-arrays",
        legacy: "integer: last applied match's new length",
        resp3: NULLS,
        notes: "three-argument form appends one value at the legacy root (ADR-0042 D7)",
    },
    ReplyShape {
        name: "JSON.ARRINSERT",
        write: true,
        dollar: "array: new length per match, null for non-arrays",
        legacy: "integer: last applied match's new length",
        resp3: NULLS,
        notes: "resolved index outside `0..=len` aborts the whole command (ADR-0042 D3)",
    },
    ReplyShape {
        name: "JSON.ARRINDEX",
        write: false,
        dollar: "array: found index or -1 per match, null for non-arrays",
        legacy: "integer: first match's found index or -1",
        resp3: NULLS,
        notes: "scalar needles only; `[start, stop)` with `stop == 0` meaning end",
    },
    ReplyShape {
        name: "JSON.ARRLEN",
        write: false,
        dollar: "array: length per match, null for non-arrays",
        legacy: "integer: first match's length",
        resp3: NULLS,
        notes: "missing key is null",
    },
    ReplyShape {
        name: "JSON.ARRPOP",
        write: true,
        dollar: "array: popped element as bulk JSON text per match, null for non-arrays \
                 and empty arrays",
        legacy: "bulk JSON text: last array match's popped element; null when it was empty",
        resp3: NULLS,
        notes: "index defaults to -1; out-of-range clamps to the nearest end (ADR-0042 D3); a \
                reply over `doc-max-reply-bytes` answers `ERR reply too large` and nothing is \
                popped",
    },
    ReplyShape {
        name: "JSON.ARRTRIM",
        write: true,
        dollar: "array: new length per match, null for non-arrays",
        legacy: "integer: last applied match's new length",
        resp3: NULLS,
        notes: "inclusive window; out-of-range clamps, never errors (ADR-0042 D3)",
    },
    ReplyShape {
        name: "JSON.OBJKEYS",
        write: false,
        dollar: "array: per match, array of key bulk strings or null for non-objects",
        legacy: "array of key bulk strings: first match",
        resp3: NULLS,
        notes: "keys in insertion order (ADR-0036); a reply over `doc-max-reply-bytes` \
                answers `ERR reply too large`",
    },
    ReplyShape {
        name: "JSON.OBJLEN",
        write: false,
        dollar: "array: entry count per match, null for non-objects",
        legacy: "integer: first match's entry count",
        resp3: NULLS,
        notes: "missing key is null",
    },
    ReplyShape {
        name: "JSON.MERGE",
        write: true,
        dollar: "`+OK`",
        legacy: "`+OK`",
        resp3: "identical",
        notes: "RFC 7386 at the selected value; null members inside object patches delete \
                keys (ADR-0042 D6); creates missing keys at the root",
    },
    ReplyShape {
        name: "JSON.DEBUG",
        write: false,
        dollar: "integer: exact attributed bytes for `MEMORY key`; missing key is null",
        legacy: "same (the command has no path mode)",
        resp3: NULLS,
        notes: "partial: shared pools and allocator slack remain in INFO memory, not per key",
    },
];

#[cfg(test)]
mod tests {
    use super::*;
    use inf_wire::Protocol;

    fn durable_cx(budget: usize, record_max: usize) -> ConnCx {
        let cx = ConnCx {
            ns: crate::exec::ConnNamespace::Named(inf_store::NsId(16)),
            ..ConnCx::try_default().expect("fixture cache allocation")
        };
        cx.node.doc_log_admission.set(Some(crate::exec::DocLogAdmission { budget, record_max }));
        cx
    }

    #[test]
    fn exact_full_image_admission_refuses_before_root_commit() {
        let argv: [&[u8]; 4] = [b"JSON.SET", b"doc", b"$", br#"{"pad":"xxxxxxxx"}"#];
        let now = Nanos::from_millis(1);
        for (budget, record_max, prefix) in [
            (1, usize::MAX, b"-BUSY".as_slice()),
            (usize::MAX, 1, b"-ERR document too large".as_slice()),
        ] {
            let mut store = CellStore::new(Default::default());
            let cx = durable_cx(budget, record_max);
            let mut out = Vec::new();
            {
                let mut writer = RespWriter::new(&mut out, Protocol::Resp2);
                execute_json(CommandId::JsonSet, &argv[..], &mut store, &cx, now, &mut writer);
            }
            assert!(out.starts_with(prefix), "reply was {}", String::from_utf8_lossy(&out));
            assert!(store.json_freeze(b"doc", now).unwrap().is_none(), "refusal committed state");
            // The BUSY refusal is the only one the `log_admission_busy`
            // gauge counts — a too-large record is a caller error, not
            // staging pressure (v0.4.0-alpha instrument fix).
            let busy = u64::from(prefix.starts_with(b"-BUSY"));
            assert_eq!(cx.node.log_admission_busy.get(), busy, "refusal counter");
        }
    }

    fn run(argv: &[&[u8]], store: &mut CellStore, cx: &ConnCx, now: Nanos) -> Vec<u8> {
        let meta = inf_wire::lookup(argv[0]).expect("a registered JSON command");
        let mut out = Vec::new();
        {
            let mut writer = RespWriter::new(&mut out, Protocol::Resp2);
            execute_json(meta.id, argv, store, cx, now, &mut writer);
        }
        out
    }

    /// `[[["y"×40000]]]`, written through the handler under an unbounded
    /// staging admission, with the doc-log scratch cleared afterwards.
    fn pop_store(budget: usize) -> (CellStore, Vec<u8>) {
        let config = inf_store::StoreConfig { doc_max_reply_bytes: budget, ..Default::default() };
        let mut store = CellStore::new(config);
        let cx = durable_cx(usize::MAX, usize::MAX);
        let element = format!("\"{}\"", "y".repeat(40_000));
        let doc = format!("[[[{element}]]]");
        let now = Nanos::from_millis(1);
        let reply = run(&[b"JSON.SET", b"p", b"$", doc.as_bytes()], &mut store, &cx, now);
        assert_eq!(reply, b"+OK\r\n");
        (store, element.into_bytes())
    }

    /// ADR-0099 A1 (R2, R5): a refused path mutation leaves no reply byte,
    /// no store write and no doc-log capture. Four raw matches of one
    /// 40 KB element are 160,052 reply bytes against a 64 KiB budget.
    #[test]
    fn refused_reply_leaves_the_document_and_the_doc_log_untouched() {
        let (mut store, _) = pop_store(64 << 10);
        let cx = durable_cx(usize::MAX, usize::MAX);
        let now = Nanos::from_millis(2);
        let before = store.json_freeze(b"p", now).unwrap().expect("fixture document");
        let reply = run(&[b"JSON.ARRPOP", b"p", b"$[0,0][0,0]"], &mut store, &cx, now);
        let shown = String::from_utf8_lossy(&reply[..reply.len().min(48)]).into_owned();
        assert!(reply == b"-ERR reply too large\r\n", "reply {} B: {shown:?}", reply.len());
        let after = store.json_freeze(b"p", now).unwrap().expect("fixture document");
        assert!(after == before, "the refused pop changed the document");
        let intent = &cx.node.doc_log.borrow().intent;
        assert!(matches!(intent, DocLogIntent::None), "doc-log intent {intent:?}");
    }

    /// ADR-0099 A1's transition table, row `Built` × `Refused(err)`: the
    /// reply is built before the commit, so a staging refusal rolls the
    /// built frame back and answers exactly its own error line.
    #[test]
    fn commit_refusal_replaces_the_built_reply() {
        let (mut store, _) = pop_store(inf_store::StoreConfig::default().doc_max_reply_bytes);
        let cx = durable_cx(1, usize::MAX);
        let now = Nanos::from_millis(2);
        let reply = run(&[b"JSON.ARRPOP", b"p", b"$[0][0]"], &mut store, &cx, now);
        let want = format!("-{}\r\n", crate::durable::STAGING_BUSY_ERROR);
        assert_eq!(String::from_utf8_lossy(&reply), want);
        assert_eq!(cx.node.log_admission_busy.get(), 1, "the refusal is the staging one");
    }

    /// A delta record counts at least one match (the log decoder refuses
    /// zero), so a committed edit whose match set cannot be that count is
    /// logged as its full post-image, which replay always accepts, never a
    /// cell panic. `apply` reports a result per raw match today; the test
    /// empties the set by hand to reach the one state the type rules out.
    #[test]
    fn a_committed_edit_without_a_delta_count_logs_its_full_image() {
        let mut store = CellStore::new(Default::default());
        let cx = durable_cx(usize::MAX, usize::MAX);
        let now = Nanos::from_millis(1);
        let set = run(&[b"JSON.SET", b"doc", b"$", br#"{"arr":[1]}"#], &mut store, &cx, now);
        assert_eq!(set, b"+OK\r\n");
        cx.node.doc_log.borrow_mut().clear();
        let op = ApplyOp::Clear;
        let Ok((program, mut outcome)) = mutate(&mut store, &cx, b"doc", b"$.arr", &op, now) else {
            panic!("the fixture edit plans");
        };
        assert!(outcome.document.is_some(), "the fixture edit applies");
        outcome.results.clear();
        let edit = PathEdit { key: b"doc", program: &program, op: &op, outcome: &outcome };
        let committed = commit(&mut store, &edit, &cx, now);
        assert!(matches!(committed, CommitOutcome::Committed), "the edit commits");
        let intent = &cx.node.doc_log.borrow().intent;
        assert!(matches!(intent, DocLogIntent::Full), "doc-log intent {intent:?}");
        let get = run(&[b"JSON.GET", b"doc", b"$"], &mut store, &cx, now);
        assert_eq!(String::from_utf8_lossy(&get), "$12\r\n[{\"arr\":[]}]\r\n");
    }

    /// `json_reply_refusals_cell` and `json_reply_refused_bytes_cell`
    /// (ADR-0099 A1's observables): a refused reply counts one and the
    /// bytes it built before its rollback; each refused `MGET` element
    /// counts one; a served or declined reply counts nothing; both folds
    /// saturate. `$[0,0]` over `[[1,2,3]]` answers `*2\r\n:3\r\n:3\r\n`
    /// (12 B): at 11 B the header and the first count (8 B) are written,
    /// then the second count crosses.
    #[test]
    fn reply_refusals_fold_into_this_cells_counters() {
        let config = inf_store::StoreConfig { doc_max_reply_bytes: 11, ..Default::default() };
        let mut store = CellStore::new(config);
        let cx = durable_cx(usize::MAX, usize::MAX);
        let now = Nanos::from_millis(1);
        let node = &cx.node;
        let refusals = &node.json_reply_refusals_cell;
        let refused_bytes = &node.json_reply_refused_bytes_cell;
        let counters = || (refusals.get(), refused_bytes.get());
        assert_eq!(run(&[b"JSON.SET", b"k", b"$", b"[[1,2,3]]"], &mut store, &cx, now), b"+OK\r\n");
        assert_eq!(run(&[b"JSON.ARRLEN", b"k", b"$[0]"], &mut store, &cx, now), b"*1\r\n:3\r\n");
        assert_eq!(counters(), (0, 0), "a served reply counts nothing");
        let refused = run(&[b"JSON.ARRLEN", b"k", b"$[0,0]"], &mut store, &cx, now);
        assert_eq!(refused, b"-ERR reply too large\r\n");
        assert_eq!(counters(), (1, 8), "one refusal, the 8 B it rolled back");
        let declined = run(&[b"JSON.ARRLEN", b"k", b"$["], &mut store, &cx, now);
        assert!(declined.starts_with(b"-ERR"), "{}", String::from_utf8_lossy(&declined));
        assert_eq!(counters(), (1, 8), "a declined reply counts nothing");
        // Each MGET element is its own account: `$` answers the bulk
        // `[[[1,2,3]]]` (18 B), which crosses 11 B.
        let mget = run(&[b"JSON.MGET", b"k", b"k", b"$"], &mut store, &cx, now);
        assert_eq!(mget, b"*2\r\n-ERR reply too large\r\n-ERR reply too large\r\n");
        let (refusal_count, bytes) = counters();
        assert_eq!(refusal_count, 3, "two refused elements count two");
        assert!(bytes > 8, "the refused elements' built bytes count");
        refusals.set(u64::MAX);
        refused_bytes.set(u64::MAX);
        let refused = run(&[b"JSON.ARRLEN", b"k", b"$[0,0]"], &mut store, &cx, now);
        assert_eq!(refused, b"-ERR reply too large\r\n");
        assert_eq!(counters(), (u64::MAX, u64::MAX), "both folds saturate");
    }

    /// JSON text: `levels` nested arrays around `0`.
    fn nested_arrays(levels: usize) -> String {
        format!("{}0{}", "[".repeat(levels), "]".repeat(levels))
    }

    /// JSON text: `levels` nested objects `{"a":…}` around `0`.
    fn nested_objects(levels: usize) -> String {
        format!("{}0{}", r#"{"a":"#.repeat(levels), "}".repeat(levels))
    }

    /// The canonical bytes and version of `key`.
    fn state_of(store: &mut CellStore, key: &[u8], now: Nanos) -> (Vec<u8>, u32) {
        let version = store.json_get(key, now).unwrap().expect("fixture document").version;
        (store.json_freeze(key, now).unwrap().expect("fixture document"), version)
    }

    /// ADR-0169 I5(a): a refused path mutation leaves the document's bytes
    /// and version and the doc-log intent untouched. Each deepening command
    /// composes depth 129 from an operand of at most 128 levels, and a
    /// `STRAPPEND` grows a body one byte past the stored-document bound.
    /// The executor, not `execute_json`, clears the doc-log scratch, so the
    /// test clears it after setup and before each refused command.
    #[test]
    fn path_refusals_change_nothing() {
        let mut store = CellStore::new(Default::default());
        let cx = durable_cx(usize::MAX, usize::MAX);
        let now = Nanos::from_millis(1);
        let (arrays, objects) = (nested_arrays(DEPTH_FIXTURE), nested_objects(DEPTH_FIXTURE));
        let finding = nested_arrays(DEPTH_FIXTURE + 1);
        let string_len = STORED_BODY_MAX - 10 - 99;
        let big = format!(r#"{{"s":"{}"}}"#, "x".repeat(string_len));
        let fixtures: [(&[u8], &[u8]); 5] = [
            (b"replace", br#"{"d":[0]}"#),
            (b"member", br#"{"d":{}}"#),
            (b"array", br#"{"d":[]}"#),
            (b"root", b"[]"),
            (b"size", big.as_bytes()),
        ];
        for (key, json) in fixtures {
            assert_eq!(run(&[b"JSON.SET", key, b"$", json], &mut store, &cx, now), b"+OK\r\n");
        }
        let payload = format!(r#""{}""#, "y".repeat(100));
        let nesting = b"-ERR document nesting too deep\r\n".as_slice();
        let refused: [(&[&[u8]], &[u8]); 7] = [
            (&[b"JSON.SET", b"replace", b"$.d[0]", arrays.as_bytes()], nesting),
            (&[b"JSON.SET", b"member", b"$.d.x", arrays.as_bytes()], nesting),
            (&[b"JSON.MERGE", b"replace", b"$.d[0]", objects.as_bytes()], nesting),
            (&[b"JSON.ARRAPPEND", b"array", b"$.d", arrays.as_bytes()], nesting),
            (&[b"JSON.ARRINSERT", b"array", b"$.d", b"0", arrays.as_bytes()], nesting),
            (&[b"JSON.ARRAPPEND", b"root", b"$", finding.as_bytes()], nesting),
            (
                &[b"JSON.STRAPPEND", b"size", b"$.s", payload.as_bytes()],
                b"-ERR document too large\r\n",
            ),
        ];
        for (argv, want) in refused {
            let before = state_of(&mut store, argv[1], now);
            cx.node.doc_log.borrow_mut().clear();
            let reply = run(argv, &mut store, &cx, now);
            let shown = String::from_utf8_lossy(&argv[..2].join(&b' ')).into_owned();
            assert_eq!(String::from_utf8_lossy(&reply), String::from_utf8_lossy(want), "{shown}");
            assert!(
                state_of(&mut store, argv[1], now) == before,
                "{shown}: the refusal changed it"
            );
            let intent = &cx.node.doc_log.borrow().intent;
            assert!(matches!(intent, DocLogIntent::None), "{shown}: doc-log intent {intent:?}");
        }
    }

    /// The operand nesting that composes depth 129 at every fixture site
    /// above: two enclosing containers (`$.d[0]`, `$.d.x`, `$.d`'s elements).
    const DEPTH_FIXTURE: usize = 127;
    /// The largest stored body: the record value cap less the document value
    /// prefix (15) and the idoc header (8).
    const STORED_BODY_MAX: usize = (1 << 24) - 1 - 15 - 8;
}
