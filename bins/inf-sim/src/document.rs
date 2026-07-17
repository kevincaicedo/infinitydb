//! M3-S23/S24 document DST machinery (ADR-0045): the merge-heavy,
//! fuzz-corpus-bearing document workload model, the read-only
//! delta-replay equivalence oracle, and power-cut class disclosure.
//!
//! The oracle is a **passive observer** (ADR-0045 D1): it reconstructs a
//! shadow keyspace per cell by reading the shared [`SimDisk`] directly
//! (manifest → named `.ick` → segment tail walk) and applying every
//! surviving record through the **production replay applier**
//! (`Keyspace::apply_record` — the ADR-0043 D6 lineage/witness arms), then
//! compares against the live cell state with §3.4 R3's currency: the
//! layout-independent `StateDigest` plus a per-document walk over
//! (lineage, version, canonical idoc bytes, canonical serialization —
//! the E8 comparator, ADR-0039 D4). `open_cell_log` is deliberately not
//! used here: recovery truncates tails and GCs segments — a mutating
//! oracle is a heisen-oracle.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use inf_doc::{JsonParser, TapeDoc, serialize_canonical_into};
use inf_foundation::rng::Entropy;
use inf_foundation::time::Nanos;
use inf_log::ckpt::{IckReaderConfig, ick_file_name, read_ick};
use inf_log::{
    Manifest, ReaderConfig, RecordView, SegmentId, SegmentReader, read_manifest, scan_log_dir_from,
};
use inf_server::{SimDisk, load_catalog_from};
use inf_store::{CheckpointImage, Keyspace, NsId, StoreConfig, WallAnchor};

use crate::durable::{DurableScenario, Node, Pending, Writer, bulk, encode};

// ---- the fuzz-derived document corpus (M3-S24) ------------------------------

/// Canonical-size ceiling for corpus documents: deliberately crosses the
/// 512 B inline and 4 KiB morph thresholds (tree-form documents meet the
/// checkpoint walker) while keeping the worst-case group-commit frame
/// under the document scenario's 64 KiB segments (ADR-0045 D3).
const CORPUS_CANONICAL_MAX: usize = 6 * 1024;

/// Deterministic cap on distinct corpus documents in the workload pool.
const CORPUS_DOC_CAP: usize = 256;

/// One minimized fuzz input that parses under default limits, held as its
/// canonical text — the exact bytes `JSON.GET` must reproduce after the
/// document crosses parse → mutate → log → crash → recover → serialize.
pub(crate) struct CorpusDoc {
    pub(crate) canonical_text: Vec<u8>,
}

/// Loads the minimized `json_parse` corpus once per process, sorted by
/// file name (git-pinned content ⇒ deterministic pool; ADR-0045 D3).
/// Panics when the checkout has no usable corpus: a silently empty pool
/// would claim fuzz coverage the run never had (L10).
pub(crate) fn corpus_documents() -> &'static [CorpusDoc] {
    static CORPUS: OnceLock<Vec<CorpusDoc>> = OnceLock::new();
    CORPUS.get_or_init(|| {
        let dir =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../crates/inf-doc/fuzz/corpora/json");
        let mut names: Vec<PathBuf> = std::fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("fuzz corpus dir {}: {e}", dir.display()))
            .filter_map(|entry| entry.ok().map(|e| e.path()))
            .filter(|path| path.is_file())
            .collect();
        names.sort();
        let mut parser = JsonParser::new();
        let mut seen = BTreeSet::new();
        let mut docs = Vec::new();
        for path in names {
            let Ok(bytes) = std::fs::read(&path) else { continue };
            let Ok(idoc) = parser.parse(&bytes) else { continue };
            let doc = TapeDoc::from_bytes(&idoc).expect("parser output validates");
            let mut text = Vec::new();
            serialize_canonical_into(doc.root().into(), &mut text);
            if text.is_empty() || text.len() > CORPUS_CANONICAL_MAX {
                continue;
            }
            if seen.insert(text.clone()) {
                docs.push(CorpusDoc { canonical_text: text });
            }
            if docs.len() == CORPUS_DOC_CAP {
                break;
            }
        }
        assert!(!docs.is_empty(), "minimized fuzz corpus yielded no parse-valid documents");
        docs
    })
}

// ---- the harness-side document model (exact expectations) ------------------

/// The harness model of one workload document. Member order is fixed by
/// construction — `values`, `meta{tag[,on]}`, `[blob]`, `[x]` — because
/// merges only replace members in place or append at the end (ADR-0045
/// D3), so `render()` is the exact `JSON.GET` reply and the §8.2
/// admissible-state oracle keeps binding without consulting `inf-doc`'s
/// mutation engine.
#[derive(Clone, Debug)]
pub(crate) struct DocModel {
    values: (i64, i64),
    tag: i64,
    on: Option<bool>,
    x: Option<i64>,
    /// Index into [`corpus_documents`] — the fuzz-derived subtree.
    blob: Option<u32>,
}

impl DocModel {
    fn render(&self) -> Vec<u8> {
        let corpus = corpus_documents();
        let mut out = Vec::with_capacity(64);
        out.extend_from_slice(
            format!(
                "{{\"values\":[{},{}],\"meta\":{{\"tag\":{}",
                self.values.0, self.values.1, self.tag
            )
            .as_bytes(),
        );
        if let Some(on) = self.on {
            out.extend_from_slice(if on { b",\"on\":true" } else { b",\"on\":false" });
        }
        out.push(b'}');
        if let Some(blob) = self.blob {
            out.extend_from_slice(b",\"blob\":");
            out.extend_from_slice(&corpus[blob as usize].canonical_text);
        }
        if let Some(x) = self.x {
            out.extend_from_slice(format!(",\"x\":{x}").as_bytes());
        }
        out.push(b'}');
        out
    }
}

fn ok_pending(key: Vec<u8>, state: Vec<u8>) -> Pending {
    Pending {
        key,
        state_after: Some(state),
        expect: b"+OK\r\n".to_vec(),
        mutates: true,
        taints: false,
    }
}

/// Builds the next document command + its exact expected reply — the
/// merge-heavy mix (S14's named S23 hand-off): root `JSON.SET`
/// (post-image, every other one carrying a fuzz-corpus blob), multi-match
/// `NUMINCRBY` (one `DocDelta` per command — S18's structural atomicity),
/// three RFC 7386 `MERGE` classes, a path `JSON.SET $.blob` subtree
/// replace, a root-member null delete, and `JSON.GET` audits.
pub(crate) fn next_document_command(
    writer: &mut Writer,
    scenario: &DurableScenario,
) -> (Vec<u8>, Pending) {
    let key = writer.key(scenario.keys_per_writer);
    let corpus = corpus_documents();
    if !writer.models.contains_key(&key) || writer.sent.is_multiple_of(96) {
        let blob = if writer.rng.next_below(2) == 0 {
            writer.corpus_docs_used += 1;
            Some(writer.rng.next_below(corpus.len() as u64) as u32)
        } else {
            None
        };
        let seed_value = (writer.id as i64 + 1) * 1_000 + writer.sent as i64;
        let model =
            DocModel { values: (seed_value, seed_value), tag: 0, on: Some(true), x: None, blob };
        let state = model.render();
        let wire = encode(&[b"JSON.SET", &key, b"$", &state]);
        writer.models.insert(key.clone(), model);
        return (wire, ok_pending(key, state));
    }
    let model = writer.models.get_mut(&key).expect("existing document has a model");
    let roll = writer.rng.next_below(100);
    if roll < 10 {
        let state = model.render();
        let wire = encode(&[b"JSON.GET", &key]);
        let pending = Pending {
            key,
            state_after: Some(state.clone()),
            expect: bulk(&state),
            mutates: false,
            taints: false,
        };
        return (wire, pending);
    }
    if roll < 40 {
        model.values.0 += 1;
        model.values.1 += 1;
        let answer = format!("[{},{}]", model.values.0, model.values.1);
        let state = model.render();
        let wire = encode(&[b"JSON.NUMINCRBY", &key, b"$.values[*]", b"1"]);
        let pending = Pending {
            key,
            state_after: Some(state),
            expect: bulk(answer.as_bytes()),
            mutates: true,
            taints: false,
        };
        return (wire, pending);
    }
    if roll < 58 {
        // RFC 7386 nested member set: {"meta":{"tag":K}} recurses into the
        // object and replaces `tag` in place.
        model.tag = writer.rng.next_below(1_000_000) as i64;
        let patch = format!("{{\"meta\":{{\"tag\":{}}}}}", model.tag);
        let state = model.render();
        let wire = encode(&[b"JSON.MERGE", &key, b"$", patch.as_bytes()]);
        return (wire, ok_pending(key, state));
    }
    if roll < 72 {
        // Null member delete + root member add in one patch: `on` leaves
        // `meta`, `x` sets (first add appends last — the order invariant).
        model.on = None;
        model.x = Some(writer.rng.next_below(1_000_000) as i64);
        let patch = format!("{{\"meta\":{{\"on\":null}},\"x\":{}}}", model.x.expect("just set"));
        let state = model.render();
        let wire = encode(&[b"JSON.MERGE", &key, b"$", patch.as_bytes()]);
        return (wire, ok_pending(key, state));
    }
    if roll < 82 {
        // Member re-add after delete: `on` re-enters `meta` at the end,
        // which is exactly where the model renders it.
        let on = writer.rng.next_below(2) == 0;
        model.on = Some(on);
        let patch = format!("{{\"meta\":{{\"on\":{on}}}}}");
        let state = model.render();
        let wire = encode(&[b"JSON.MERGE", &key, b"$", patch.as_bytes()]);
        return (wire, ok_pending(key, state));
    }
    if roll < 92 && model.blob.is_some() {
        // Path-targeted subtree replace: another fuzz document lands via
        // one `DocDelta` (or a cadence `DocFull` when the operand out-sizes
        // the document — the ADR-0043 D5 rule meets real inputs here).
        writer.corpus_docs_used += 1;
        let blob = writer.rng.next_below(corpus.len() as u64) as u32;
        model.blob = Some(blob);
        let state = model.render();
        let wire = encode(&[b"JSON.SET", &key, b"$.blob", &corpus[blob as usize].canonical_text]);
        return (wire, ok_pending(key, state));
    }
    // Root-member null delete (no-op when `blob` is already absent — RFC
    // 7386 removing a missing member).
    model.blob = None;
    let state = model.render();
    let wire = encode(&[b"JSON.MERGE", &key, b"$", b"{\"blob\":null}"]);
    (wire, ok_pending(key, state))
}

// ---- the delta-replay equivalence oracle (M3-S23) ---------------------------

/// What the oracle tallied across a run (report disclosure — a dead
/// oracle must be visible, so checks and compared documents are counted).
#[derive(Debug, Default)]
pub(crate) struct EquivalenceStats {
    pub(crate) checks: u64,
    pub(crate) documents_compared: u64,
}

/// One document's layout-independent identity: §3.4 R3's currency.
struct DocImage {
    lineage: u64,
    version: u32,
    idoc: Vec<u8>,
}

/// Walks one cell's persisted log tail with **prefix semantics** — stop
/// at the first unreadable frame, exactly like recovery: a recovered
/// (truncated) torn segment legitimately reads as valid-prefix-then-
/// zeros while later pre-cut segment files still exist on disk. A
/// too-short shadow from a mid-log stop is not silent — it surfaces as
/// state divergence in the compare.
fn walk_tail(
    disk: &SimDisk,
    log_dir: &Path,
    floor: SegmentId,
    begin: Option<inf_log::Lsn>,
    mut on_record: impl FnMut(&RecordView<'_>) -> Result<(), String>,
) -> Result<(), String> {
    let scan =
        scan_log_dir_from(disk, log_dir, floor).map_err(|e| format!("log scan: {e:?}"))?.scan;
    'segments: for &segment in scan.segments() {
        let Ok(mut reader) = SegmentReader::open(disk, log_dir, segment, ReaderConfig::default())
        else {
            break 'segments;
        };
        loop {
            match reader.next_frame() {
                Ok(Some(frame)) => {
                    for record in frame.records() {
                        let Ok((lsn, record)) = record else { break 'segments };
                        if begin.is_some_and(|b| lsn < b) {
                            continue;
                        }
                        on_record(&record)?;
                    }
                }
                Ok(None) => break,
                Err(_) => break 'segments,
            }
        }
    }
    Ok(())
}

/// Replays one cell's persisted prefix into `shadow` through the
/// production applier, read-only (ADR-0045 D1). `canary` skips the
/// **last** tail `DocDelta` — the newest delta is the one least likely
/// to be covered by a later full image, so the planted bug diverges on
/// almost every seed (skipping the first would usually drop a delta the
/// applier already skips as checkpoint-stale — a toothless canary).
fn replay_shadow(
    disk: &SimDisk,
    data_dir: &Path,
    cell: u16,
    shadow: &mut Keyspace,
    now: Nanos,
    canary: &mut bool,
) -> Result<(), String> {
    let anchor = WallAnchor { internal_ms: 0, unix_ms: 0 };
    let shard = data_dir.join(format!("shard-{cell}"));
    let manifest =
        read_manifest(disk, &shard).map_err(|e| format!("cell {cell}: manifest: {e}"))?;
    if let Some(manifest) = &manifest {
        let ick = shard.join("ckpt").join(ick_file_name(manifest.ckpt_id));
        read_ick(disk, &ick, IckReaderConfig::default(), |record| {
            shadow.apply_record(&record, now, anchor).map(|_| ()).map_err(|e| format!("{e:?}"))
        })
        .map_err(|e| format!("cell {cell}: checkpoint: {e:?}"))?;
    }
    let begin = manifest.as_ref().map(|m| m.begin_lsn);
    let floor = manifest.as_ref().map_or(SegmentId(0), Manifest::floor);
    let log_dir = shard.join("log");
    let mut skip_delta_index = None;
    if *canary {
        let mut delta_count = 0u64;
        walk_tail(disk, &log_dir, floor, begin, |record| {
            delta_count += u64::from(matches!(record, RecordView::DocDelta { .. }));
            Ok(())
        })
        .map_err(|e| format!("cell {cell}: {e}"))?;
        if delta_count > 0 {
            skip_delta_index = Some(delta_count - 1);
            *canary = false;
        }
    }
    let mut delta_seen = 0u64;
    walk_tail(disk, &log_dir, floor, begin, |record| {
        if matches!(record, RecordView::DocDelta { .. }) {
            let index = delta_seen;
            delta_seen += 1;
            if skip_delta_index == Some(index) {
                return Ok(());
            }
        }
        shadow.apply_record(record, now, anchor).map(|_| ()).map_err(|e| format!("apply: {e:?}"))
    })
    .map_err(|e| format!("cell {cell}: {e}"))?;
    Ok(())
}

/// Collects a store's live documents as layout-independent images. The
/// walk freezes to plain canonical idoc bytes (ADR-0038 D3), so tape,
/// arena, and interned physical forms are indistinguishable here.
fn document_images(store: &inf_store::CellStore, now: Nanos) -> BTreeMap<Vec<u8>, DocImage> {
    let mut images = BTreeMap::new();
    let mut cursor = 0u64;
    loop {
        cursor = store.digest_checkpoint_images(cursor, 1024, now, |key, image, _expire| {
            if let CheckpointImage::JsonDoc { lineage, version, idoc } = image {
                let image = DocImage { lineage: lineage.get(), version, idoc: idoc.to_vec() };
                images.insert(key.to_vec(), image);
            }
        });
        if cursor == 0 {
            return images;
        }
    }
}

fn canonical_text(idoc: &[u8]) -> Result<Vec<u8>, String> {
    let doc = TapeDoc::from_bytes(idoc).map_err(|e| format!("{e:?}"))?;
    let mut text = Vec::new();
    serialize_canonical_into(doc.root().into(), &mut text);
    Ok(text)
}

fn preview(text: &[u8]) -> String {
    let cut = text.len().min(120);
    format!("{}{}", String::from_utf8_lossy(&text[..cut]), if text.len() > cut { "…" } else { "" })
}

/// Byte-exact per-document comparison: lineage, version, canonical idoc
/// bytes, and the E8 canonical serialization (ADR-0039 D4). Violations
/// name the seed, instant, cell, namespace, and key — a 10k-seed sweep
/// that only says "mismatch" is unactionable.
#[allow(clippy::too_many_arguments)] // violation context: seed + instant + cell + namespace
fn compare_documents(
    seed: u64,
    label: &str,
    cell: u16,
    ns_name: &str,
    live: &BTreeMap<Vec<u8>, DocImage>,
    shadow: &BTreeMap<Vec<u8>, DocImage>,
    stats: &mut EquivalenceStats,
    violations: &mut Vec<String>,
) {
    let context = format!("seed {seed:#x} [{label}] cell {cell} ns {ns_name}");
    let mut fail = |key: &[u8], what: String| {
        violations.push(format!(
            "REPLAY EQUIVALENCE VIOLATION {context} key {:?}: {what}",
            String::from_utf8_lossy(key)
        ));
    };
    for (key, live_image) in live {
        let Some(shadow_image) = shadow.get(key) else {
            fail(key, "live document absent from the replayed shadow".to_string());
            continue;
        };
        stats.documents_compared += 1;
        if live_image.lineage != shadow_image.lineage {
            fail(
                key,
                format!(
                    "lineage diverged: live {} vs replayed {}",
                    live_image.lineage, shadow_image.lineage
                ),
            );
        }
        if live_image.version != shadow_image.version {
            fail(
                key,
                format!(
                    "version diverged: live {} vs replayed {}",
                    live_image.version, shadow_image.version
                ),
            );
        }
        let live_text = canonical_text(&live_image.idoc);
        let shadow_text = canonical_text(&shadow_image.idoc);
        match (live_text, shadow_text) {
            (Ok(live_text), Ok(shadow_text)) => {
                if live_text != shadow_text {
                    fail(
                        key,
                        format!(
                            "canonical serialization diverged: live {} vs replayed {}",
                            preview(&live_text),
                            preview(&shadow_text)
                        ),
                    );
                } else if live_image.idoc != shadow_image.idoc {
                    // Equal canonical text with unequal frozen bytes would
                    // mean the canonical encoding is not unique — a format
                    // bug, not a replay bug; still a finding.
                    fail(key, "canonical idoc bytes diverged under equal text".to_string());
                }
            }
            (live_text, shadow_text) => {
                fail(key, format!("undecodable frozen idoc: {live_text:?} / {shadow_text:?}"));
            }
        }
    }
    for key in shadow.keys() {
        if !live.contains_key(key) {
            fail(key, "replayed document absent from the live store".to_string());
        }
    }
}

/// One equivalence check over every cell (M3-S23): live state vs the
/// read-only shadow replay. `label` names the instant ("mid-run-N" /
/// "post-recovery") in every violation.
pub(crate) fn equivalence_check(
    scenario: &DurableScenario,
    label: &str,
    node: &Node,
    disk: &SimDisk,
    now: Nanos,
    stats: &mut EquivalenceStats,
    violations: &mut Vec<String>,
) {
    let data_dir = PathBuf::from("node");
    let catalog = match load_catalog_from(disk, &data_dir) {
        Ok(Some(catalog)) => catalog,
        other => {
            violations.push(format!(
                "REPLAY EQUIVALENCE VIOLATION seed {:#x} [{label}]: catalog unreadable ({other:?})",
                scenario.seed
            ));
            return;
        }
    };
    let mut canary = scenario.replay_canary;
    for cell in 0..scenario.cells {
        let mut shadow = Keyspace::new(StoreConfig::default());
        if let Err(err) = shadow.seed_catalog(&catalog) {
            violations.push(format!(
                "REPLAY EQUIVALENCE VIOLATION seed {:#x} [{label}]: seed_catalog: {err:?}",
                scenario.seed
            ));
            return;
        }
        if let Err(err) = replay_shadow(disk, &data_dir, cell, &mut shadow, now, &mut canary) {
            violations.push(format!(
                "REPLAY EQUIVALENCE VIOLATION seed {:#x} [{label}]: shadow replay: {err}",
                scenario.seed
            ));
            continue;
        }
        let live = node.plane(cell as usize).keyspace();
        // The whole-state catch-all: canonical idoc + lineage + version
        // per document, values + expiry per string (ADR-0043 D7).
        if live.state_digest(now) != shadow.state_digest(now) {
            violations.push(format!(
                "REPLAY EQUIVALENCE VIOLATION seed {:#x} [{label}] cell {cell}: state digest \
                 diverged (live {:?} vs replayed {:?})",
                scenario.seed,
                live.state_digest(now),
                shadow.state_digest(now)
            ));
        }
        for spec in &catalog.entries {
            let ns: NsId = spec.id;
            let ns_name = String::from_utf8_lossy(&spec.name).into_owned();
            let (Some(live_store), Some(shadow_store)) = (live.ns_store(ns), shadow.ns_store(ns))
            else {
                continue;
            };
            let live_images = document_images(live_store, now);
            let shadow_images = document_images(shadow_store, now);
            compare_documents(
                scenario.seed,
                label,
                cell,
                &ns_name,
                &live_images,
                &shadow_images,
                stats,
                violations,
            );
        }
    }
    stats.checks += 1;
}

// ---- power-cut class disclosure (M3-S24) ------------------------------------

fn record_class(record: &RecordView<'_>) -> &'static str {
    match record {
        RecordView::StringPostImage { .. } => "string",
        RecordView::Delete { .. } => "delete",
        RecordView::ExpireAt { .. } => "expire",
        RecordView::NsOp { .. } => "ns-op",
        RecordView::CkptBegin { .. } => "ckpt-begin",
        RecordView::DocDelta { .. } => "doc-delta",
        RecordView::DocFull { .. } => "doc-full",
    }
}

/// Classifies the surviving log's cut boundary per cell — the class of
/// the last durable record — and returns the deduplicated, sorted set.
/// Coverage is disclosed, never assumed (ADR-0045 D4): sweeps aggregate
/// the distribution, and the CI slice asserts both document classes occur.
pub(crate) fn classify_cut(disk: &SimDisk, data_dir: &Path, cells: u16) -> Vec<&'static str> {
    let mut classes = BTreeSet::new();
    for cell in 0..cells {
        let shard = data_dir.join(format!("shard-{cell}"));
        let Ok(manifest) = read_manifest(disk, &shard) else {
            classes.insert("unreadable");
            continue;
        };
        let floor = manifest.as_ref().map_or(SegmentId(0), Manifest::floor);
        let log_dir = shard.join("log");
        let Ok(outcome) = scan_log_dir_from(disk, &log_dir, floor) else {
            classes.insert("unreadable");
            continue;
        };
        let mut last = "none";
        'segments: for &segment in outcome.scan.segments() {
            let Ok(mut reader) =
                SegmentReader::open(disk, &log_dir, segment, ReaderConfig::default())
            else {
                break 'segments;
            };
            loop {
                match reader.next_frame() {
                    Ok(Some(frame)) => {
                        for record in frame.records() {
                            let Ok((_lsn, record)) = record else { break 'segments };
                            last = record_class(&record);
                        }
                    }
                    Ok(None) => break,
                    Err(_) => break 'segments,
                }
            }
        }
        classes.insert(last);
    }
    classes.into_iter().collect()
}
