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
//! Mutations run the §3.4 R4 discipline end-to-end: `inf_doc::apply`
//! validates the whole match set before producing bytes, and the store
//! rewrite (`json_replace` — one version bump) happens only when an edit
//! actually applied. A failed command leaves value, version, and
//! accounting untouched; a no-op command (all matches skipped) never
//! rewrites.

use inf_doc::apply::{ApplyError, ApplyOp, ApplyOutcome, MatchResult, Number, apply};
use inf_doc::path::{EvalLimits, Matches, PathProgram, Segment, eval, resolve};
use inf_doc::ser::{Reply, SerializeOpts, serialize_into, serialize_reply_into};
use inf_doc::{DeltaOpcode, DocValue, JsonErrorKind, ObjCursor, TapeDoc, encode_apply_op};
use inf_foundation::time::Nanos;
use inf_store::{CellStore, JsonScalarPatch, JsonSetOptions, JsonSetOutcome, SetCond, SetExpire};
use inf_wire::{CommandId, Protocol, RespWriter};

use crate::exec::{Argv, ConnCx, op_error, parse_i64};

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
        match_count: u32,
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

    fn delta(&mut self, program: &PathProgram, op: &ApplyOp<'_>, match_count: u32) {
        assert!(match_count > 0, "a logged document delta changed at least one match");
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
fn capture_delta(cx: &ConnCx, program: &PathProgram, op: &ApplyOp<'_>, match_count: u32) {
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
/// current document has no TTL.
fn durable_full_fits(cx: &ConnCx, key: &[u8], idoc: &[u8], w: &mut RespWriter<'_>) -> bool {
    let Some(admission) = cx.node.doc_log_admission.get() else {
        return true;
    };
    let ns = cx.ns.expect("durable capture is named-namespace-only");
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
        w.error("ERR document too large for durable log staging");
        return false;
    }
    if full.saturating_add(expiry) > admission.budget {
        // Counted with the owner-side `would_fit` refusals
        // (`log_admission_busy`): same typed reply, same
        // invisible-to-staging pre-check shape.
        cx.node.log_admission_busy.set(cx.node.log_admission_busy.get() + 1);
        w.error(crate::durable::STAGING_BUSY_ERROR);
        return false;
    }
    true
}

pub(crate) fn execute_json(
    id: CommandId,
    argv: &(impl Argv + ?Sized),
    store: &mut CellStore,
    cx: &ConnCx,
    now: Nanos,
    w: &mut RespWriter<'_>,
) {
    match id {
        CommandId::JsonSet => set(argv, store, cx, now, w),
        CommandId::JsonGet => get(argv, store, cx, now, w),
        CommandId::JsonMget => mget(argv, store, cx, now, w),
        CommandId::JsonDel | CommandId::JsonForget => del(argv, store, cx, now, w),
        CommandId::JsonType => type_of(argv, store, cx, now, w),
        CommandId::JsonNumIncrBy | CommandId::JsonNumMultBy => {
            num_op(id, argv, store, cx, now, w);
        }
        CommandId::JsonStrAppend => str_append(argv, store, cx, now, w),
        CommandId::JsonStrLen => str_len(argv, store, cx, now, w),
        CommandId::JsonToggle => toggle(argv, store, cx, now, w),
        CommandId::JsonClear => clear(argv, store, cx, now, w),
        CommandId::JsonArrAppend => arr_append(argv, store, cx, now, w),
        CommandId::JsonArrInsert => arr_insert(argv, store, cx, now, w),
        CommandId::JsonArrIndex => arr_index(argv, store, cx, now, w),
        CommandId::JsonArrLen => arr_len(argv, store, cx, now, w),
        CommandId::JsonArrPop => arr_pop(argv, store, cx, now, w),
        CommandId::JsonArrTrim => arr_trim(argv, store, cx, now, w),
        CommandId::JsonObjKeys => obj_keys(argv, store, cx, now, w),
        CommandId::JsonObjLen => obj_len(argv, store, cx, now, w),
        CommandId::JsonMerge => merge(argv, store, cx, now, w),
        CommandId::JsonDebug => debug(argv, store, now, w),
        _ => unreachable!("execute_db routes exactly the JSON family here"),
    }
}

// ---- JSON.DEBUG -------------------------------------------------------------

fn debug(argv: &(impl Argv + ?Sized), store: &mut CellStore, now: Nanos, w: &mut RespWriter<'_>) {
    if !argv.arg(1).eq_ignore_ascii_case(b"MEMORY") {
        return w.error("ERR unknown JSON.DEBUG subcommand");
    }
    match store.json_memory_usage(argv.arg(2), now) {
        Ok(Some(bytes)) => {
            w.int(i64::try_from(bytes).expect("one document is bounded far below i64::MAX"));
        }
        Ok(None) => w.null(),
        Err(error) => op_error(error, w),
    }
}

// ---- shared plumbing --------------------------------------------------------

/// Compile through the per-cell cache (S10), cloning the program out of
/// the cache borrow. `PathProgram` owns cell-local `Rc` bytes, so a cache
/// hit clone is allocation-free without holding the `RefCell` guard across
/// later command work (ADR-0043 D1).
fn compile(
    store: &CellStore,
    cx: &ConnCx,
    text: &[u8],
    w: &mut RespWriter<'_>,
) -> Option<PathProgram> {
    let mut cache = cx.node.path_cache.borrow_mut();
    match cache.get_or_compile(text, store.doc_max_path_bytes()) {
        Ok(program) => Some(program.clone()),
        Err(e) => {
            w.error(&format!("ERR {e}"));
            None
        }
    }
}

fn eval_limits(store: &CellStore) -> EvalLimits {
    EvalLimits { max_matches: store.doc_max_path_matches() }
}

/// Parse a JSON value argument with the target store's resolved limits
/// (ADR-0039 D5's per-namespace resolution) into the recycled per-cell
/// ingest buffer (the S05 lever-G seam). `None` ⇒ the error is written.
fn parse_value(
    store: &CellStore,
    cx: &ConnCx,
    text: &[u8],
    out: &mut Vec<u8>,
    w: &mut RespWriter<'_>,
) -> bool {
    let mut parser = cx.node.json_parser.borrow_mut();
    parser.set_limits(store.doc_parse_limits());
    match parser.parse_into(text, out) {
        Ok(()) => true,
        Err(e) => {
            // The two limit rejections carry their ADR-0039 D5 pinned
            // phrasing; everything else reports the typed offset line.
            match e.kind {
                JsonErrorKind::DocumentTooLarge => w.error("ERR document too large"),
                JsonErrorKind::DepthExceeded => w.error("ERR document nesting too deep"),
                _ => w.error(&format!("ERR invalid JSON: {e}")),
            }
            false
        }
    }
}

fn apply_error(e: ApplyError, w: &mut RespWriter<'_>) {
    match e {
        ApplyError::TooLarge => w.error("ERR document too large"),
        ApplyError::Eval(inner) => w.error(&format!("ERR {inner}")),
        other => w.error(&format!("ERR {other}")),
    }
}

fn path_missing(path: &[u8], w: &mut RespWriter<'_>) {
    let path = String::from_utf8_lossy(path);
    w.error(&format!("ERR Path '{path}' does not exist"));
}

const MISSING_KEY: &str = "ERR could not perform this operation on a key that doesn't exist";

/// Freeze a document's plain canonical bytes for a path mutation, or
/// write the command's missing-key/WRONGTYPE reply. The freeze copy is
/// the interim ADR-0041 D5 backend (S16 owns the in-place fast path).
fn frozen_doc(
    store: &mut CellStore,
    key: &[u8],
    now: Nanos,
    missing: impl FnOnce(&mut RespWriter<'_>),
    w: &mut RespWriter<'_>,
) -> Option<Vec<u8>> {
    match store.json_freeze(key, now) {
        Ok(Some(bytes)) => Some(bytes),
        Ok(None) => {
            missing(w);
            None
        }
        Err(e) => {
            op_error(e, w);
            None
        }
    }
}

/// Commit a mutation outcome: rewrite + one version bump when an edit
/// applied; a no-op leaves the record untouched (ADR-0041 D8).
fn commit(
    store: &mut CellStore,
    key: &[u8],
    outcome: &ApplyOutcome,
    now: Nanos,
    w: &mut RespWriter<'_>,
) -> bool {
    let Some(bytes) = &outcome.bytes else { return true };
    match store.json_replace(key, bytes, now) {
        Ok(replaced) => {
            debug_assert!(replaced, "the key was resolved by the freeze above");
            true
        }
        Err(e) => {
            op_error(e, w);
            false
        }
    }
}

/// Commit and capture one logical path edit. No-op outcomes remain absent
/// from the log, matching the version rule: no bytes, no bump, no record.
#[allow(clippy::too_many_arguments)]
fn commit_delta(
    store: &mut CellStore,
    key: &[u8],
    program: &PathProgram,
    op: &ApplyOp<'_>,
    outcome: &ApplyOutcome,
    cx: &ConnCx,
    now: Nanos,
    w: &mut RespWriter<'_>,
) -> bool {
    let changed = outcome.bytes.is_some();
    if let Some(bytes) = &outcome.bytes
        && !durable_full_fits(cx, key, bytes, w)
    {
        return false;
    }
    if !commit(store, key, outcome, now, w) {
        return false;
    }
    if changed {
        capture_delta(
            cx,
            program,
            op,
            u32::try_from(outcome.results.len()).expect("match set is capped at u32"),
        );
    }
    true
}

/// The last applied (non-skipped) match in raw order — the legacy
/// single-value mutation reply (ADR-0041 D7; S21 oracle-verified).
fn last_applied(results: &[MatchResult]) -> Option<MatchResult> {
    results.iter().rev().find(|r| !matches!(r, MatchResult::Skipped)).copied()
}

fn write_number_value(n: Number, w: &mut RespWriter<'_>) {
    if w.protocol() == Protocol::Resp3 {
        return match n {
            Number::I64(value) => w.int(value),
            Number::F64(value) => w.double(value),
        };
    }
    let mut text = [0u8; 32];
    let len = inf_doc::serialize_number_text(n, &mut text);
    w.bulk(&text[..len]);
}

// ---- JSON.SET ---------------------------------------------------------------

fn set(
    argv: &(impl Argv + ?Sized),
    store: &mut CellStore,
    cx: &ConnCx,
    now: Nanos,
    w: &mut RespWriter<'_>,
) {
    let cond = match argv.len() {
        4 => SetCond::Always,
        5 if argv.arg(4).eq_ignore_ascii_case(b"NX") => SetCond::IfAbsent,
        5 if argv.arg(4).eq_ignore_ascii_case(b"XX") => SetCond::IfPresent,
        _ => return w.error("ERR syntax error"),
    };
    let (key, path) = (argv.arg(1), argv.arg(2));
    let Some(program) = compile(store, cx, path, w) else { return };
    let mut idoc = cx.node.json_ingest_buf.take();
    let parsed = parse_value(store, cx, argv.arg(3), &mut idoc, w);
    let outcome = parsed.then(|| set_parsed(store, key, path, &program, cond, &idoc, cx, now, w));
    cx.node.json_ingest_buf.replace(idoc);
    let Some(Some(applied)) = outcome else { return };
    if applied {
        if program.is_root() {
            capture_full(cx);
        }
        w.simple("OK");
    } else {
        w.null();
    }
}

/// The post-parse half of `JSON.SET`: root sets are post-images
/// (`json_set`); path sets run replace-or-create per ADR-0041 D6.
/// `None` ⇒ the error reply is written; `Some(applied)` maps to OK/null.
#[allow(clippy::too_many_arguments)]
fn set_parsed(
    store: &mut CellStore,
    key: &[u8],
    path: &[u8],
    program: &PathProgram,
    cond: SetCond,
    idoc: &[u8],
    cx: &ConnCx,
    now: Nanos,
    w: &mut RespWriter<'_>,
) -> Option<bool> {
    if program.is_root() {
        if !durable_full_fits(cx, key, idoc, w) {
            return None;
        }
        // Root TTL semantics: preserved (the key is replaced, not recreated).
        let opts = JsonSetOptions { cond, expire: SetExpire::Keep };
        return match store.json_set(key, idoc, opts, now) {
            Ok(JsonSetOutcome::Applied) => Some(true),
            Ok(JsonSetOutcome::Skipped) => Some(false),
            Err(e) => {
                op_error(e, w);
                None
            }
        };
    }
    let frozen =
        frozen_doc(store, key, now, |w| w.error("ERR new objects must be created at the root"), w)?;
    let doc = TapeDoc::from_validated_bytes(&frozen);
    let limits = eval_limits(store);
    let matches = match eval(program, DocValue::from(doc.root()), &limits) {
        Ok(m) => m,
        Err(e) => {
            w.error(&format!("ERR {e}"));
            return None;
        }
    };
    match cond {
        SetCond::IfAbsent if !matches.is_empty() => return Some(false),
        SetCond::IfPresent if matches.is_empty() => return Some(false),
        _ => {}
    }
    let fragment = &idoc[inf_doc::HEADER_LEN..];
    let op = if matches.is_empty() {
        // Creation: only a plain final child name creates, on every
        // matched parent object (ADR-0041 D6).
        let ast = inf_doc::path::parse_ast(path).expect("compile above accepted this text");
        let Some(Segment::Child(name)) = ast.segments.last() else {
            path_missing(path, w);
            return None;
        };
        let parent = inf_doc::path::encode_ast(&inf_doc::path::PathAst {
            legacy: ast.legacy,
            segments: ast.segments[..ast.segments.len() - 1].to_vec(),
        });
        let name = name.clone();
        return set_apply(
            store,
            key,
            path,
            &doc,
            &parent,
            &ApplyOp::SetMember { key: &name, fragment },
            &limits,
            cx,
            now,
            w,
        );
    } else {
        ApplyOp::SetReplace { fragment }
    };
    set_apply(store, key, path, &doc, program, &op, &limits, cx, now, w)
}

#[allow(clippy::too_many_arguments)]
fn set_apply(
    store: &mut CellStore,
    key: &[u8],
    path: &[u8],
    doc: &TapeDoc<'_>,
    program: &PathProgram,
    op: &ApplyOp<'_>,
    limits: &EvalLimits,
    cx: &ConnCx,
    now: Nanos,
    w: &mut RespWriter<'_>,
) -> Option<bool> {
    match apply(doc, program, op, limits, store.doc_max_bytes()) {
        Ok(outcome) if outcome.bytes.is_some() => {
            commit_delta(store, key, program, op, &outcome, cx, now, w).then_some(true)
        }
        Ok(_) => {
            // No eligible site (every parent skipped): the frozen
            // path-does-not-exist arm.
            path_missing(path, w);
            None
        }
        Err(e) => {
            apply_error(e, w);
            None
        }
    }
}

// ---- JSON.GET / JSON.MGET ---------------------------------------------------

/// Parsed `JSON.GET` argument tail: formatting options + path list.
struct GetArgs<'a> {
    opts: SerializeOpts<'a>,
    paths: Vec<&'a [u8]>,
}

fn parse_get_args<'a>(
    argv: &'a (impl Argv + ?Sized),
    w: &mut RespWriter<'_>,
) -> Option<GetArgs<'a>> {
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
                w.error("ERR syntax error");
                return None;
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
    Some(GetArgs { opts, paths })
}

fn get(
    argv: &(impl Argv + ?Sized),
    store: &mut CellStore,
    cx: &ConnCx,
    now: Nanos,
    w: &mut RespWriter<'_>,
) {
    let Some(args) = parse_get_args(argv, w) else { return };
    // Compile every path before touching the store (cheap misses beat a
    // half-evaluated command), collecting owned programs.
    let mut programs = Vec::with_capacity(args.paths.len());
    for path in &args.paths {
        let Some(program) = compile(store, cx, path, w) else { return };
        programs.push(program);
    }
    let limits = eval_limits(store);
    let read = match store.json_get(argv.arg(1), now) {
        Ok(Some(read)) => read,
        Ok(None) => return w.null(),
        Err(e) => return op_error(e, w),
    };
    let mut match_sets: Vec<Matches> = Vec::with_capacity(programs.len());
    for (program, path) in programs.iter().zip(&args.paths) {
        match eval(program, read.root, &limits) {
            Ok(m) => {
                if program.is_legacy() && m.is_empty() {
                    return path_missing(path, w);
                }
                match_sets.push(m);
            }
            Err(e) => return w.error(&format!("ERR {e}")),
        }
    }
    let reply = if programs.len() == 1 {
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
    w.bulk_patched(|out| serialize_reply_into(&reply, &args.opts, out));
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
    w: &mut RespWriter<'_>,
) {
    let path = argv.arg(argv.len() - 1);
    let Some(program) = compile(store, cx, path, w) else { return };
    let limits = eval_limits(store);
    w.array_header(argv.len() - 2);
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
            Some(reply) => {
                w.bulk_patched(|out| {
                    serialize_reply_into(&reply, &SerializeOpts::default(), out);
                });
            }
            None => w.null(),
        }
    }
}

// ---- JSON.DEL / JSON.FORGET / JSON.TYPE --------------------------------------

fn del(
    argv: &(impl Argv + ?Sized),
    store: &mut CellStore,
    cx: &ConnCx,
    now: Nanos,
    w: &mut RespWriter<'_>,
) {
    let key = argv.arg(1);
    let path: &[u8] = if argv.len() > 2 { argv.arg(2) } else { b"." };
    let Some(program) = compile(store, cx, path, w) else { return };
    if program.is_root() {
        // Root deletion is key-level lifecycle (kernel-owned Delete
        // record at S17), never a path edit — after the type gate.
        return match store.type_of(key, now) {
            Some(inf_store::TypeTag::JsonDoc) => {
                let deleted = store.del(key, now);
                if deleted {
                    capture_delete(cx);
                }
                w.int(i64::from(deleted));
            }
            Some(_) => op_error(inf_store::OpError::WrongType, w),
            None => w.int(0),
        };
    }
    let Some(frozen) = frozen_doc(store, key, now, |w| w.int(0), w) else { return };
    let doc = TapeDoc::from_validated_bytes(&frozen);
    match apply(&doc, &program, &ApplyOp::Del, &eval_limits(store), store.doc_max_bytes()) {
        Ok(outcome) => {
            if commit_delta(store, key, &program, &ApplyOp::Del, &outcome, cx, now, w) {
                w.int(i64::from(outcome.applied));
            }
        }
        Err(e) => apply_error(e, w),
    }
}

fn type_of(
    argv: &(impl Argv + ?Sized),
    store: &mut CellStore,
    cx: &ConnCx,
    now: Nanos,
    w: &mut RespWriter<'_>,
) {
    let path: &[u8] = if argv.len() > 2 { argv.arg(2) } else { b"." };
    let Some(program) = compile(store, cx, path, w) else { return };
    let limits = eval_limits(store);
    let read = match store.json_get(argv.arg(1), now) {
        Ok(Some(read)) => read,
        Ok(None) => return w.null(),
        Err(e) => return op_error(e, w),
    };
    let matches = match eval(&program, read.root, &limits) {
        Ok(m) => m,
        Err(e) => return w.error(&format!("ERR {e}")),
    };
    let name = |i: usize| {
        type_name(resolve(read.root, matches.get(i)).expect("matches resolve on their document"))
    };
    match (w.protocol(), program.is_legacy()) {
        (Protocol::Resp2, true) => match matches.is_empty() {
            true => w.null(),
            false => w.bulk(name(0).as_bytes()),
        },
        (Protocol::Resp2, false) => {
            w.array_header(matches.len());
            for i in 0..matches.len() {
                w.bulk(name(i).as_bytes());
            }
        }
        (Protocol::Resp3, true) => {
            w.array_header(1);
            match matches.is_empty() {
                true => w.null(),
                false => w.bulk(name(0).as_bytes()),
            }
        }
        (Protocol::Resp3, false) => {
            w.array_header(matches.len());
            for i in 0..matches.len() {
                w.array_header(1);
                w.bulk(name(i).as_bytes());
            }
        }
    }
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

fn num_op(
    id: CommandId,
    argv: &(impl Argv + ?Sized),
    store: &mut CellStore,
    cx: &ConnCx,
    now: Nanos,
    w: &mut RespWriter<'_>,
) {
    let (key, path) = (argv.arg(1), argv.arg(2));
    let Some(operand) = parse_number_operand(argv.arg(3)) else {
        return w.error("ERR value is not a number");
    };
    let op = match id {
        CommandId::JsonNumIncrBy => ApplyOp::NumIncrBy(operand),
        _ => ApplyOp::NumMultBy(operand),
    };
    let Some(program) = compile(store, cx, path, w) else { return };
    match store.json_patch_scalar(key, &program, &op, now) {
        Ok(Some(JsonScalarPatch::Number(number))) => {
            capture_delta(cx, &program, &op, 1);
            return write_fast_number(path, program.is_legacy(), Some(number), w);
        }
        Ok(Some(JsonScalarPatch::Missing)) => {
            return write_fast_number(path, program.is_legacy(), None, w);
        }
        Ok(Some(JsonScalarPatch::Skipped)) => {
            return write_fast_number_skipped(path, program.is_legacy(), w);
        }
        Ok(Some(JsonScalarPatch::Unsupported)) => {}
        Ok(Some(JsonScalarPatch::Toggled(_))) => unreachable!("numeric probe returns a number"),
        Ok(None) => return w.error(MISSING_KEY),
        Err(inf_store::OpError::Overflow) => return apply_error(ApplyError::Overflow, w),
        Err(inf_store::OpError::NanOrInf) => return apply_error(ApplyError::NotANumber, w),
        Err(e) => return op_error(e, w),
    }
    let Some(outcome) = mutate_with(store, key, &program, &op, now, w) else { return };
    if !commit_delta(store, key, &program, &op, &outcome, cx, now, w) {
        return;
    }
    if program.is_legacy() {
        return match last_applied(&outcome.results) {
            Some(MatchResult::Num(n)) => {
                if w.protocol() == Protocol::Resp3 {
                    w.array_header(1);
                }
                write_number_value(n, w);
            }
            Some(_) => unreachable!("numeric ops apply numbers"),
            None if outcome.results.is_empty() => path_missing(path, w),
            None => {
                let path = String::from_utf8_lossy(path);
                w.error(&format!("ERR Path '{path}' does not contain a number"));
            }
        };
    }
    if w.protocol() == Protocol::Resp3 {
        w.array_header(outcome.results.len());
        for result in &outcome.results {
            match result {
                MatchResult::Num(number) => write_number_value(*number, w),
                _ => w.null(),
            }
        }
    } else {
        let members = outcome
            .results
            .iter()
            .map(|r| match r {
                MatchResult::Num(Number::I64(v)) => Reply::Value(DocValue::I64(*v)),
                MatchResult::Num(Number::F64(v)) => Reply::Value(DocValue::F64(*v)),
                _ => Reply::Value(DocValue::Null),
            })
            .collect();
        let reply = Reply::Array(members);
        w.bulk_patched(|out| serialize_reply_into(&reply, &SerializeOpts::default(), out));
    }
}

/// Operand of NUMINCRBY/NUMMULTBY: a standalone JSON number token parsed
/// by the ingest parser's shared grammar without constructing an idoc.
fn parse_number_operand(text: &[u8]) -> Option<Number> {
    inf_doc::parse_number_token(text).ok()
}

fn write_fast_number(path: &[u8], legacy: bool, number: Option<Number>, w: &mut RespWriter<'_>) {
    if legacy && number.is_none() {
        return path_missing(path, w);
    }
    if w.protocol() == Protocol::Resp3 {
        w.array_header(usize::from(legacy || number.is_some()));
        if let Some(number) = number {
            write_number_value(number, w);
        }
        return;
    }
    if legacy {
        return match number {
            Some(number) => write_number_value(number, w),
            None => path_missing(path, w),
        };
    }
    let mut payload = [0u8; 34];
    payload[0] = b'[';
    let mut len = 1;
    if let Some(number) = number {
        let mut text = [0u8; 32];
        let text_len = inf_doc::serialize_number_text(number, &mut text);
        payload[len..len + text_len].copy_from_slice(&text[..text_len]);
        len += text_len;
    }
    payload[len] = b']';
    w.bulk(&payload[..=len]);
}

fn write_fast_number_skipped(path: &[u8], legacy: bool, w: &mut RespWriter<'_>) {
    if legacy {
        let path = String::from_utf8_lossy(path);
        return w.error(&format!("ERR Path '{path}' does not contain a number"));
    }
    if w.protocol() == Protocol::Resp3 {
        w.array_header(1);
        w.null();
    } else {
        w.bulk(b"[null]");
    }
}

fn str_append(
    argv: &(impl Argv + ?Sized),
    store: &mut CellStore,
    cx: &ConnCx,
    now: Nanos,
    w: &mut RespWriter<'_>,
) {
    let key = argv.arg(1);
    let (path, value): (&[u8], &[u8]) = if argv.len() == 3 {
        (b".", argv.arg(2)) // The RedisJSON-compatible implicit-root quirk.
    } else {
        (argv.arg(2), argv.arg(3))
    };
    let mut parser = inf_doc::JsonParser::new();
    let Ok(operand_doc) = parser.parse(value) else {
        return w.error("ERR value is not a string");
    };
    let operand = TapeDoc::from_validated_bytes(&operand_doc);
    let DocValue::Str(payload) = DocValue::from(operand.root()) else {
        return w.error("ERR value is not a string");
    };
    let op = ApplyOp::StrAppend(payload.as_bytes());
    let Some((program, outcome)) = mutate(store, cx, key, path, &op, now, w) else { return };
    if !commit_delta(store, key, &program, &op, &outcome, cx, now, w) {
        return;
    }
    int_per_match(path, program.is_legacy(), &outcome, w, "a string", |r| match r {
        MatchResult::Len(n) => Some(*n as i64),
        _ => None,
    });
}

fn str_len(
    argv: &(impl Argv + ?Sized),
    store: &mut CellStore,
    cx: &ConnCx,
    now: Nanos,
    w: &mut RespWriter<'_>,
) {
    int_read(argv, store, cx, now, w, "a string", |v| match v {
        DocValue::Str(s) => Some(s.as_bytes().len() as i64),
        _ => None,
    });
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
    w: &mut RespWriter<'_>,
    noun: &str,
    project: impl Fn(DocValue<'_>) -> Option<i64>,
) {
    let path: &[u8] = if argv.len() > 2 { argv.arg(2) } else { b"." };
    let Some(program) = compile(store, cx, path, w) else { return };
    let limits = eval_limits(store);
    let read = match store.json_get(argv.arg(1), now) {
        Ok(Some(read)) => read,
        Ok(None) => return w.null(),
        Err(e) => return op_error(e, w),
    };
    let matches = match eval(&program, read.root, &limits) {
        Ok(m) => m,
        Err(e) => return w.error(&format!("ERR {e}")),
    };
    let value_of = |i: usize| resolve(read.root, matches.get(i)).and_then(&project);
    if program.is_legacy() {
        if matches.is_empty() {
            return path_missing(path, w);
        }
        return match value_of(0) {
            Some(n) => w.int(n),
            None => {
                let path = String::from_utf8_lossy(path);
                w.error(&format!("ERR Path '{path}' does not contain {noun}"));
            }
        };
    }
    w.array_header(matches.len());
    for i in 0..matches.len() {
        match value_of(i) {
            Some(n) => w.int(n),
            None => w.null(),
        }
    }
}

fn toggle(
    argv: &(impl Argv + ?Sized),
    store: &mut CellStore,
    cx: &ConnCx,
    now: Nanos,
    w: &mut RespWriter<'_>,
) {
    let key = argv.arg(1);
    let path: &[u8] = if argv.len() > 2 { argv.arg(2) } else { b"." };
    let op = ApplyOp::Toggle;
    let Some(program) = compile(store, cx, path, w) else { return };
    match store.json_patch_scalar(key, &program, &op, now) {
        Ok(Some(JsonScalarPatch::Toggled(value))) => {
            capture_delta(cx, &program, &op, 1);
            if program.is_legacy() {
                w.bulk(if value { b"true" } else { b"false" });
            } else {
                w.array_header(1);
                w.int(i64::from(value));
            }
            return;
        }
        Ok(Some(JsonScalarPatch::Missing)) => {
            if program.is_legacy() {
                path_missing(path, w);
            } else {
                w.array_header(0);
            }
            return;
        }
        Ok(Some(JsonScalarPatch::Skipped)) => {
            if program.is_legacy() {
                let path = String::from_utf8_lossy(path);
                w.error(&format!("ERR Path '{path}' does not contain a boolean"));
            } else {
                w.array_header(1);
                w.null();
            }
            return;
        }
        Ok(Some(JsonScalarPatch::Unsupported)) => {}
        Ok(Some(JsonScalarPatch::Number(_))) => unreachable!("toggle probe returns a boolean"),
        Ok(None) => return w.error(MISSING_KEY),
        Err(e) => return op_error(e, w),
    }
    let Some(outcome) = mutate_with(store, key, &program, &op, now, w) else { return };
    if !commit_delta(store, key, &program, &op, &outcome, cx, now, w) {
        return;
    }
    if program.is_legacy() {
        return match last_applied(&outcome.results) {
            Some(MatchResult::Toggled(b)) => w.bulk(if b { b"true" } else { b"false" }),
            Some(_) => unreachable!("toggle applies booleans"),
            None if outcome.results.is_empty() => path_missing(path, w),
            None => {
                let path = String::from_utf8_lossy(path);
                w.error(&format!("ERR Path '{path}' does not contain a boolean"));
            }
        };
    }
    w.array_header(outcome.results.len());
    for r in &outcome.results {
        match r {
            MatchResult::Toggled(b) => w.int(i64::from(*b)),
            _ => w.null(),
        }
    }
}

fn clear(
    argv: &(impl Argv + ?Sized),
    store: &mut CellStore,
    cx: &ConnCx,
    now: Nanos,
    w: &mut RespWriter<'_>,
) {
    let key = argv.arg(1);
    let path: &[u8] = if argv.len() > 2 { argv.arg(2) } else { b"." };
    let op = ApplyOp::Clear;
    let Some((program, outcome)) = mutate(store, cx, key, path, &op, now, w) else {
        return;
    };
    if commit_delta(store, key, &program, &op, &outcome, cx, now, w) {
        w.int(i64::from(outcome.applied));
    }
}

// ---- array ops (M3-S13, ADR-0042 D1–D4) ---------------------------------------

fn arr_append(
    argv: &(impl Argv + ?Sized),
    store: &mut CellStore,
    cx: &ConnCx,
    now: Nanos,
    w: &mut RespWriter<'_>,
) {
    let key = argv.arg(1);
    // The optional-path quirk (ADR-0042 D7): three arguments mean a
    // legacy root path and a single value — the STRAPPEND precedent.
    let (path, first_value): (&[u8], usize) =
        if argv.len() == 3 { (b".", 2) } else { (argv.arg(2), 3) };
    let Some(operand) = parse_array_operand(store, cx, argv, first_value, w) else { return };
    let op = ApplyOp::ArrAppend { elements: &operand };
    let Some((program, outcome)) = mutate(store, cx, key, path, &op, now, w) else { return };
    if !commit_delta(store, key, &program, &op, &outcome, cx, now, w) {
        return;
    }
    int_per_match(path, program.is_legacy(), &outcome, w, "an array", array_len_result);
}

fn arr_insert(
    argv: &(impl Argv + ?Sized),
    store: &mut CellStore,
    cx: &ConnCx,
    now: Nanos,
    w: &mut RespWriter<'_>,
) {
    let (key, path) = (argv.arg(1), argv.arg(2));
    let Ok(index) = parse_i64(argv.arg(3)) else {
        return w.error("ERR value is not an integer or out of range");
    };
    let Some(operand) = parse_array_operand(store, cx, argv, 4, w) else { return };
    let op = ApplyOp::ArrInsert { index, elements: &operand };
    let Some((program, outcome)) = mutate(store, cx, key, path, &op, now, w) else { return };
    if !commit_delta(store, key, &program, &op, &outcome, cx, now, w) {
        return;
    }
    int_per_match(path, program.is_legacy(), &outcome, w, "an array", array_len_result);
}

fn arr_trim(
    argv: &(impl Argv + ?Sized),
    store: &mut CellStore,
    cx: &ConnCx,
    now: Nanos,
    w: &mut RespWriter<'_>,
) {
    let (key, path) = (argv.arg(1), argv.arg(2));
    let (Ok(start), Ok(stop)) = (parse_i64(argv.arg(3)), parse_i64(argv.arg(4))) else {
        return w.error("ERR value is not an integer or out of range");
    };
    let op = ApplyOp::ArrTrim { start, stop };
    let Some((program, outcome)) = mutate(store, cx, key, path, &op, now, w) else { return };
    if !commit_delta(store, key, &program, &op, &outcome, cx, now, w) {
        return;
    }
    int_per_match(path, program.is_legacy(), &outcome, w, "an array", array_len_result);
}

fn array_len_result(r: &MatchResult) -> Option<i64> {
    match r {
        MatchResult::Len(n) => Some(*n as i64),
        _ => None,
    }
}

fn arr_pop(
    argv: &(impl Argv + ?Sized),
    store: &mut CellStore,
    cx: &ConnCx,
    now: Nanos,
    w: &mut RespWriter<'_>,
) {
    let key = argv.arg(1);
    let path: &[u8] = if argv.len() > 2 { argv.arg(2) } else { b"." };
    let mut index = -1i64;
    if argv.len() > 3 {
        let Ok(parsed) = parse_i64(argv.arg(3)) else {
            return w.error("ERR value is not an integer or out of range");
        };
        index = parsed;
    }
    // Inlined mutation prologue: the reply serializes popped elements
    // from the frozen pre-image (`MatchResult::Popped` offsets are only
    // meaningful against it — ADR-0042 D4), so `frozen` must outlive the
    // commit instead of dying inside `mutate`.
    let Some(program) = compile(store, cx, path, w) else { return };
    let Some(frozen) = frozen_doc(store, key, now, |w| w.error(MISSING_KEY), w) else { return };
    let doc = TapeDoc::from_validated_bytes(&frozen);
    let op = ApplyOp::ArrPop { index };
    let outcome = match apply(&doc, &program, &op, &eval_limits(store), store.doc_max_bytes()) {
        Ok(outcome) => outcome,
        Err(e) => return apply_error(e, w),
    };
    if !commit_delta(store, key, &program, &op, &outcome, cx, now, w) {
        return;
    }
    let popped_text = |at: u32, out: &mut Vec<u8>| {
        serialize_into(DocValue::from(doc.value_at(at as usize)), &SerializeOpts::default(), out);
    };
    if program.is_legacy() {
        // Last array match wins: its popped value, or null when it was
        // empty; no array match at all takes the type/path error arms.
        let last_array = outcome
            .results
            .iter()
            .rev()
            .find(|r| matches!(r, MatchResult::Popped(_) | MatchResult::PoppedEmpty));
        return match last_array {
            Some(MatchResult::Popped(at)) => {
                let mut text = Vec::new();
                popped_text(*at, &mut text);
                w.bulk(&text);
            }
            Some(_) => w.null(),
            None if outcome.results.is_empty() => path_missing(path, w),
            None => {
                let path = String::from_utf8_lossy(path);
                w.error(&format!("ERR Path '{path}' does not contain an array"));
            }
        };
    }
    w.array_header(outcome.results.len());
    for r in &outcome.results {
        match r {
            MatchResult::Popped(at) => {
                let mut text = Vec::new();
                popped_text(*at, &mut text);
                w.bulk(&text);
            }
            _ => w.null(),
        }
    }
}

fn arr_len(
    argv: &(impl Argv + ?Sized),
    store: &mut CellStore,
    cx: &ConnCx,
    now: Nanos,
    w: &mut RespWriter<'_>,
) {
    int_read(argv, store, cx, now, w, "an array", |v| match v {
        DocValue::Arr(a) => Some(a.len() as i64),
        _ => None,
    });
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
    w: &mut RespWriter<'_>,
) {
    let (key, path) = (argv.arg(1), argv.arg(2));
    let Some(needle) = parse_scalar_needle(argv.arg(3)) else {
        return w.error("ERR value is not a scalar");
    };
    let mut range = [0i64, 0i64];
    for (slot, i) in range.iter_mut().zip(4..argv.len()) {
        let Ok(parsed) = parse_i64(argv.arg(i)) else {
            return w.error("ERR value is not an integer or out of range");
        };
        *slot = parsed;
    }
    let Some(program) = compile(store, cx, path, w) else { return };
    let limits = eval_limits(store);
    let read = match store.json_get(key, now) {
        Ok(Some(read)) => read,
        Ok(None) => return w.null(),
        Err(e) => return op_error(e, w),
    };
    let matches = match eval(&program, read.root, &limits) {
        Ok(m) => m,
        Err(e) => return w.error(&format!("ERR {e}")),
    };
    let index_of = |i: usize| match resolve(read.root, matches.get(i)) {
        Some(DocValue::Arr(a)) => Some(array_search(&a, &needle, range[0], range[1])),
        _ => None,
    };
    if program.is_legacy() {
        if matches.is_empty() {
            return path_missing(path, w);
        }
        return match index_of(0) {
            Some(n) => w.int(n),
            None => {
                let path = String::from_utf8_lossy(path);
                w.error(&format!("ERR Path '{path}' does not contain an array"));
            }
        };
    }
    w.array_header(matches.len());
    for i in 0..matches.len() {
        match index_of(i) {
            Some(n) => w.int(n),
            None => w.null(),
        }
    }
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
    w: &mut RespWriter<'_>,
) {
    int_read(argv, store, cx, now, w, "an object", |v| match v {
        DocValue::Obj(o) => Some(o.len() as i64),
        _ => None,
    });
}

fn obj_keys(
    argv: &(impl Argv + ?Sized),
    store: &mut CellStore,
    cx: &ConnCx,
    now: Nanos,
    w: &mut RespWriter<'_>,
) {
    let path: &[u8] = if argv.len() > 2 { argv.arg(2) } else { b"." };
    let Some(program) = compile(store, cx, path, w) else { return };
    let limits = eval_limits(store);
    let read = match store.json_get(argv.arg(1), now) {
        Ok(Some(read)) => read,
        Ok(None) => return w.null(),
        Err(e) => return op_error(e, w),
    };
    let matches = match eval(&program, read.root, &limits) {
        Ok(m) => m,
        Err(e) => return w.error(&format!("ERR {e}")),
    };
    let object_of = |i: usize| match resolve(read.root, matches.get(i)) {
        Some(DocValue::Obj(o)) => Some(o),
        _ => None,
    };
    let write_keys = |o: &ObjCursor<'_>, w: &mut RespWriter<'_>| {
        // Insertion order — the only order the format has (ADR-0036).
        w.array_header(o.len());
        for (key, _) in o.iter() {
            w.bulk(key.as_bytes());
        }
    };
    if program.is_legacy() {
        if matches.is_empty() {
            return path_missing(path, w);
        }
        return match object_of(0) {
            Some(o) => write_keys(&o, w),
            None => {
                let path = String::from_utf8_lossy(path);
                w.error(&format!("ERR Path '{path}' does not contain an object"));
            }
        };
    }
    w.array_header(matches.len());
    for i in 0..matches.len() {
        match object_of(i) {
            Some(o) => write_keys(&o, w),
            None => w.null(),
        }
    }
}

fn merge(
    argv: &(impl Argv + ?Sized),
    store: &mut CellStore,
    cx: &ConnCx,
    now: Nanos,
    w: &mut RespWriter<'_>,
) {
    let (key, path) = (argv.arg(1), argv.arg(2));
    let Some(program) = compile(store, cx, path, w) else { return };
    let mut idoc = cx.node.json_ingest_buf.take();
    let parsed = parse_value(store, cx, argv.arg(3), &mut idoc, w);
    let outcome = parsed.then(|| merge_parsed(store, key, path, &program, &idoc, cx, now, w));
    cx.node.json_ingest_buf.replace(idoc);
    if let Some(Some(())) = outcome {
        w.simple("OK");
    }
}

/// The post-parse half of `JSON.MERGE` (ADR-0042 D6): existing matches
/// merge in place; a missing key creates at the root only; an existing
/// key with no matches follows the SET parent-creation rule with the
/// null-stripped patch. `None` ⇒ the error reply is written.
#[allow(clippy::too_many_arguments)]
fn merge_parsed(
    store: &mut CellStore,
    key: &[u8],
    path: &[u8],
    program: &PathProgram,
    idoc: &[u8],
    cx: &ConnCx,
    now: Nanos,
    w: &mut RespWriter<'_>,
) -> Option<()> {
    let fragment = &idoc[inf_doc::HEADER_LEN..];
    if program.is_root() && store.json_get(key, now).ok().flatten().is_none() {
        // Missing key, root path: create with MergePatch(absent, patch).
        // Wrong types surface through json_set's guard below.
        let created = inf_doc::merge_absent_document(fragment);
        if !durable_full_fits(cx, key, &created, w) {
            return None;
        }
        let opts = JsonSetOptions { cond: SetCond::Always, expire: SetExpire::Keep };
        return match store.json_set(key, &created, opts, now) {
            Ok(JsonSetOutcome::Applied) => {
                capture_full(cx);
                Some(())
            }
            Ok(JsonSetOutcome::Skipped) => unreachable!("unconditional set applies"),
            Err(e) => {
                op_error(e, w);
                None
            }
        };
    }
    let frozen =
        frozen_doc(store, key, now, |w| w.error("ERR new objects must be created at the root"), w)?;
    let doc = TapeDoc::from_validated_bytes(&frozen);
    let limits = eval_limits(store);
    let matches = match eval(program, DocValue::from(doc.root()), &limits) {
        Ok(m) => m,
        Err(e) => {
            w.error(&format!("ERR {e}"));
            return None;
        }
    };
    if matches.is_empty() {
        // The SET parent-creation rule (ADR-0041 D6), with the merged-
        // against-absent value (nulls stripped through object chains).
        let ast = inf_doc::path::parse_ast(path).expect("compile above accepted this text");
        let Some(Segment::Child(name)) = ast.segments.last() else {
            path_missing(path, w);
            return None;
        };
        let parent = inf_doc::path::encode_ast(&inf_doc::path::PathAst {
            legacy: ast.legacy,
            segments: ast.segments[..ast.segments.len() - 1].to_vec(),
        });
        let name = name.clone();
        let created = inf_doc::merge_absent_document(fragment);
        let op = ApplyOp::SetMember { key: &name, fragment: &created[inf_doc::HEADER_LEN..] };
        return merge_apply(store, key, path, &doc, &parent, &op, &limits, cx, now, w);
    }
    let op = ApplyOp::Merge { patch: fragment };
    merge_apply(store, key, path, &doc, program, &op, &limits, cx, now, w)
}

/// Run one merge-family apply + commit; a no-op merge (byte-equal
/// output, ADR-0041 D8) is still `+OK`.
#[allow(clippy::too_many_arguments)]
fn merge_apply(
    store: &mut CellStore,
    key: &[u8],
    path: &[u8],
    doc: &TapeDoc<'_>,
    program: &PathProgram,
    op: &ApplyOp<'_>,
    limits: &EvalLimits,
    cx: &ConnCx,
    now: Nanos,
    w: &mut RespWriter<'_>,
) -> Option<()> {
    match apply(doc, program, op, limits, store.doc_max_bytes()) {
        Ok(outcome) => {
            if matches!(op, ApplyOp::SetMember { .. }) && outcome.bytes.is_none() {
                // Zero eligible parents: the oracle's path error arm.
                path_missing(path, w);
                return None;
            }
            commit_delta(store, key, program, op, &outcome, cx, now, w).then_some(())
        }
        Err(e) => {
            apply_error(e, w);
            None
        }
    }
}

/// Parse the trailing value arguments and wrap them as the single
/// ADR-0042 D2 canonical array operand. `None` ⇒ the error is written.
fn parse_array_operand(
    store: &CellStore,
    cx: &ConnCx,
    argv: &(impl Argv + ?Sized),
    first_value: usize,
    w: &mut RespWriter<'_>,
) -> Option<Vec<u8>> {
    let mut docs: Vec<Vec<u8>> = Vec::with_capacity(argv.len() - first_value);
    for i in first_value..argv.len() {
        let mut out = Vec::new();
        if !parse_value(store, cx, argv.arg(i), &mut out, w) {
            return None;
        }
        docs.push(out);
    }
    let fragments: Vec<&[u8]> = docs.iter().map(|d| &d[inf_doc::HEADER_LEN..]).collect();
    let operand = inf_doc::array_operand(&fragments);
    if operand.is_none() {
        w.error("ERR document too large");
    }
    operand
}

/// The shared mutation prologue: compile, freeze, apply. `None` ⇒ the
/// reply (missing key, WRONGTYPE, path/eval/apply error) is written.
/// Returns the compiled program too — reply shaping branches on its
/// recorded mode (ADR-0040: mode lives on the program, never re-derived
/// from text).
fn mutate(
    store: &mut CellStore,
    cx: &ConnCx,
    key: &[u8],
    path: &[u8],
    op: &ApplyOp<'_>,
    now: Nanos,
    w: &mut RespWriter<'_>,
) -> Option<(PathProgram, ApplyOutcome)> {
    let program = compile(store, cx, path, w)?;
    let outcome = mutate_with(store, key, &program, op, now, w)?;
    Some((program, outcome))
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
    w: &mut RespWriter<'_>,
) -> Option<ApplyOutcome> {
    let frozen = frozen_doc(store, key, now, |w| w.error(MISSING_KEY), w)?;
    let doc = TapeDoc::from_validated_bytes(&frozen);
    match apply(&doc, program, op, &eval_limits(store), store.doc_max_bytes()) {
        Ok(outcome) => Some(outcome),
        Err(e) => {
            apply_error(e, w);
            None
        }
    }
}

/// `$`-mode per-match integer array (STRAPPEND/ARR* mutations) or the
/// legacy last-match integer; skipped matches answer nulls / the pinned
/// `does not contain a {noun}` type error.
fn int_per_match(
    path: &[u8],
    legacy: bool,
    outcome: &ApplyOutcome,
    w: &mut RespWriter<'_>,
    noun: &str,
    project: impl Fn(&MatchResult) -> Option<i64>,
) {
    if legacy {
        return match last_applied(&outcome.results).as_ref().and_then(&project) {
            Some(n) => w.int(n),
            None if outcome.results.is_empty() => path_missing(path, w),
            None => {
                let path = String::from_utf8_lossy(path);
                w.error(&format!("ERR Path '{path}' does not contain {noun}"));
            }
        };
    }
    w.array_header(outcome.results.len());
    for r in &outcome.results {
        match project(r) {
            Some(n) => w.int(n),
            None => w.null(),
        }
    }
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
        notes: "i64 overflow / non-finite results abort the whole command (§3.4 R4)",
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
        notes: "index defaults to -1; out-of-range clamps to the nearest end (ADR-0042 D3)",
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
        notes: "keys in insertion order (ADR-0036)",
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
        let cx = ConnCx { ns: Some(inf_store::NsId(16)), ..ConnCx::default() };
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
}
