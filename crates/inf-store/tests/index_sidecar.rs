//! M4.5-S06 — the sidecar loader's correctness suite (plan S06 ACs;
//! schema + semantics ADR-0078, constraints ADR-0073).
//!
//! # The case table (written before the tests)
//!
//! | # | leg | outcome |
//! |---|-----|---------|
//! | 1 | fuzzy emission (mutations racing the cursor) → fresh boot → load → CatchUp tail → commit | every index `Loaded`; trees ≡ the from-scratch oracle; no jobs |
//! | 2 | generation mismatch (rebuilt after the checkpoint) | `Rebuilt{generation-mismatch}`, tree empty, S05 rebuild converges |
//! | 3 | encoding-version mismatch | `Rebuilt{encoding-version}` + rebuild |
//! | 4 | key-scheme mismatch | `Rebuilt{scheme-mismatch}` + rebuild |
//! | 5 | ordinal gap across sections | `Rebuilt{non-contiguous}` + rebuild |
//! | 6 | cross-section key regression | `Rebuilt{out-of-order}` + rebuild |
//! | 7 | stream without FINAL (abandoned mid-emission) | `Rebuilt{incomplete}` + rebuild |
//! | 8 | sections after FINAL | `Rebuilt{after-final}` + rebuild |
//! | 9 | sections naming a dropped declaration | swallowed; other indexes unaffected |
//! | 10 | empty converged tree (zero-entry FINAL) | `Loaded{0}` |
//! | 11 | tail deletes / overwrites / string-overwrite deaths under CatchUp | remove-may-miss legal; converges |
//! | 12 | damaged-section notes | counted in the INFO fold, never fatal |
//!
//! `TotalMismatch` is reachable only from hand-forged bytes (the writer
//! asserts the total, and `NonContiguous` fires first on every writer-
//! producible shape) — `fuzz_index_sidecar` owns that space; the loader
//! branch is unit-visible here only through the reason vocabulary.
//!
//! Strictness: after `commit_ready` the trees are converged and the
//! live path runs `Strict` — the post-commit mutations at the end of
//! leg 1 would fire the found/fresh debug asserts on any divergence.

#![cfg(feature = "doc")]

use std::collections::BTreeSet;
use std::path::Path;

use inf_doc::JsonParser;
use inf_doc::path::{EvalLimits, compile, eval, resolve};
use inf_foundation::time::Nanos;
use inf_log::ckpt::{IckReaderConfig, SyncIckWriter, ick_file_name, read_ick_hybrid};
use inf_log::fs::SegmentFs as _;
use inf_log::fs::mem::MemFs;
use inf_log::{
    CkptConfig, DocLineage, IckIdxSidecarStep, IdxSidecarMeta, Lsn, RecordView, SegmentId,
};
use inf_store::{
    BackfillBudget, CellStore, FsyncClass, INDEX_KEY_ENCODING_VERSION, IndexId, IndexKeyBuf,
    IndexKeyType, IndexScalar, IndexSpec, IndexState, Keyspace, NsId, NsMode, NsSpec,
    OrderedCursor, SetOptions, SidecarBootDecision, SidecarLoader, SidecarRebuildReason,
    StoreConfig, WallAnchor, index_key_encode,
};

const NS: NsId = NsId(16);
const ANCHOR: WallAnchor = WallAnchor { internal_ms: 0, unix_ms: 0 };

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // SplitMix64 — seeds in the test, never ambient randomness (L7).
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn parse(json: &str) -> Vec<u8> {
    JsonParser::new().parse(json.as_bytes()).expect("valid test JSON")
}

/// The S04/S05 index set: scalar chains plus the `[*]` multi-match case.
const INDEXES: &[(u32, &str, IndexKeyType)] = &[
    (1, "$.price", IndexKeyType::F64),
    (2, "$.name", IndexKeyType::Utf8),
    (3, "$.qty", IndexKeyType::I64),
    (4, "$.tags[*]", IndexKeyType::Utf8),
];

fn durable_keyspace() -> Keyspace {
    let mut ks = Keyspace::new(StoreConfig::default());
    ks.ns_create(NsSpec {
        id: NS,
        name: b"docs".to_vec(),
        mode: NsMode::Durable,
        fsync: Some(FsyncClass::Always),
        policy: None,
        maxmemory: None,
        tier: None,
    })
    .expect("namespace");
    ks
}

fn declare_indexes(ks: &mut Keyspace) {
    for &(id, path, key_type) in INDEXES {
        let program = compile(path.as_bytes()).expect("valid path").as_bytes().to_vec();
        ks.idx_create(IndexSpec {
            id: IndexId(id),
            generation: u64::from(id),
            ns: NS,
            name: format!("idx-{id}").into_bytes(),
            program,
            key_type,
            state: IndexState::Declared,
        })
        .expect("declare");
    }
}

fn random_doc(rng: &mut Rng) -> String {
    let price = match rng.below(4) {
        0 => format!("{}", rng.below(100)),
        1 => format!("{}.5", rng.below(100)),
        2 => "\"not-a-number\"".to_string(),
        _ => format!("-{}.25", rng.below(50)),
    };
    let name = match rng.below(4) {
        0 => "\"alpha\"",
        1 => "\"beta\"",
        2 => "null",
        _ => "\"gamma\"",
    };
    let qty = format!("{}", rng.below(1000));
    let tags = match rng.below(3) {
        0 => r#"["a","a","b"]"#,
        1 => r#"["x"]"#,
        _ => "[]",
    };
    format!(r#"{{"price":{price},"name":{name},"qty":{qty},"tags":{tags}}}"#)
}

fn key_of(i: u64) -> Vec<u8> {
    format!("doc:{i:04}").into_bytes()
}

/// The tail-record model: what phase-B live mutations replay as on the
/// fresh node (the log vocabulary, applied via `apply_record`).
enum TailOp {
    Doc(Vec<u8>, Vec<u8>),
    Del(Vec<u8>),
    Str(Vec<u8>, Vec<u8>),
}

/// Drive one live mutation through the bracket exactly as the plane
/// does, recording its tail-replay record.
fn live_op(ks: &mut Keyspace, rng: &mut Rng, now: Nanos, tail: &mut Vec<TailOp>) {
    let key = key_of(rng.below(160));
    match rng.below(8) {
        0..=4 => {
            let doc = parse(&random_doc(rng));
            bracketed(ks, &[&key], |s| {
                let _ = s.json_set(&key, &doc, Default::default(), now);
            });
            tail.push(TailOp::Doc(key, doc));
        }
        5..=6 => {
            bracketed(ks, &[&key], |s| s.del(&key, now));
            tail.push(TailOp::Del(key));
        }
        _ => {
            // A string image over a doc key is an overwrite death — the
            // replay arm's remove-may-miss case (leg 11).
            bracketed(ks, &[&key], |s| {
                let _ = s.set(&key, b"plain", SetOptions::default(), now);
            });
            tail.push(TailOp::Str(key, b"plain".to_vec()));
        }
    }
}

fn bracketed<R>(ks: &mut Keyspace, keys: &[&[u8]], mutate: impl FnOnce(&mut CellStore) -> R) -> R {
    ks.idx_bracket_begin(NS, keys, None).expect("reservation headroom");
    let store = ks.ns_store_mut(NS).expect("registered");
    let result = mutate(store);
    ks.idx_bracket_commit(NS, keys);
    result
}

fn backfill_to_convergence(ks: &mut Keyspace, now: Nanos) {
    for _ in 0..64 {
        let stats = ks.idx_backfill_tick(now, BackfillBudget::default());
        if stats.active == 0 {
            return;
        }
    }
    panic!("backfill did not converge in 64 ticks");
}

// ---- the from-scratch oracle (S04's, retargeted at the named ns) ----------

fn oracle_entries(
    ks: &mut Keyspace,
    path: &str,
    key_type: IndexKeyType,
    now: Nanos,
) -> BTreeSet<(Vec<u8>, u64)> {
    let store = ks.ns_store_mut(NS).expect("registered");
    let mut keys: Vec<Vec<u8>> = Vec::new();
    let mut cursor = 0u64;
    loop {
        cursor = store.scan(cursor, 512, now, |k| keys.push(k.to_vec()));
        if cursor == 0 {
            break;
        }
    }
    let mut expected = BTreeSet::new();
    let program = compile(path.as_bytes()).expect("valid path");
    for key in &keys {
        let Ok(Some(read)) = store.json_get(key, now) else { continue };
        let matches =
            eval(&program, read.root, &EvalLimits::default()).expect("test docs are small");
        let hash = CellStore::hash_key(key);
        let mut buf = IndexKeyBuf::new();
        for steps in matches.iter() {
            let Some(value) = resolve(read.root, steps) else { continue };
            let scalar = match value {
                inf_doc::DocValue::Null => IndexScalar::Null,
                inf_doc::DocValue::Bool(b) => IndexScalar::Bool(b),
                inf_doc::DocValue::I64(v) => IndexScalar::I64(v),
                inf_doc::DocValue::F64(f) => IndexScalar::F64(f),
                inf_doc::DocValue::Str(s) => IndexScalar::Utf8(s.to_str()),
                _ => continue,
            };
            if index_key_encode(key_type, scalar, &mut buf).is_ok() {
                expected.insert((buf.as_bytes().to_vec(), hash));
            }
        }
    }
    expected
}

fn tree_entries(ks: &Keyspace, id: IndexId) -> BTreeSet<(Vec<u8>, u64)> {
    let tree = ks.idx_tree(NS, id).expect("tree");
    let mut cursor = OrderedCursor::from_start();
    let mut out = BTreeSet::new();
    while let Some((key, entry_ref)) = tree.cursor_next(&mut cursor) {
        out.insert((key.to_vec(), entry_ref));
    }
    out
}

/// Serializes `ks`'s converged trees into a `.ick`-shaped file exactly
/// as the checkpoint driver does — chunked through the re-seek cursor
/// with `mutate` racing between chunks (the fuzzy window) — and returns
/// the validated sections re-read from the file.
fn emit_sidecars(
    ks: &mut Keyspace,
    chunk: u32,
    mut mutate: impl FnMut(&mut Keyspace),
    rewrite_meta: impl Fn(IdxSidecarMeta) -> IdxSidecarMeta,
) -> (MemFs, std::path::PathBuf) {
    let fs = MemFs::new();
    let dir = Path::new("/ckpt");
    fs.create_dir_all(dir).unwrap();
    let mut w = SyncIckWriter::create_v2(
        fs.clone(),
        dir,
        &CkptConfig { section_bytes: 256, ..Default::default() },
        0,
        1,
        Lsn::new(SegmentId(1), 64),
        &[NS.0],
    )
    .expect("create v2");
    let candidates = ks.idx_sidecar_candidates(NS);
    for (id, generation, fixed8, _len) in candidates {
        let meta = rewrite_meta(IdxSidecarMeta {
            ns: NS.0,
            index_id: id.0,
            generation,
            key_encoding_version: INDEX_KEY_ENCODING_VERSION,
            fixed8,
        });
        let mut cursor = OrderedCursor::from_start();
        let mut ordinal = 0u64;
        loop {
            let mut staged: Vec<(Vec<u8>, u64)> = Vec::new();
            let pulled = ks.idx_sidecar_emit(NS, id, &mut cursor, chunk, |key, entry_ref| {
                staged.push((key.to_vec(), entry_ref));
            });
            for (key, entry_ref) in &staged {
                w.append_idx_entry(&meta, ordinal, key, *entry_ref).expect("entry");
                ordinal += 1;
            }
            if pulled < chunk {
                break;
            }
            mutate(ks);
        }
        w.append_idx_final(&meta, ordinal).expect("final");
    }
    w.finish().expect("finish");
    (fs, dir.join(ick_file_name(1)))
}

/// Feeds every sidecar section of the file to `loader` against `ks`.
fn load_sections(loader: &mut SidecarLoader, ks: &mut Keyspace, fs: &MemFs, path: &Path) {
    let ks = std::cell::RefCell::new(ks);
    read_ick_hybrid(
        fs,
        path,
        IckReaderConfig::default(),
        |_| Ok::<(), ()>(()),
        |_| Ok(()),
        |_| Ok(()),
        |_| Ok(()),
        |step| {
            match step {
                IckIdxSidecarStep::Section(section) => {
                    loader.apply_section(&mut ks.borrow_mut(), &section);
                }
                IckIdxSidecarStep::Damaged { .. } => loader.note_damaged(),
            }
            Ok(())
        },
    )
    .expect("sidecar-bearing file loads");
}

/// A fresh "rebooted" node: same catalog, phase-A corpus replayed with
/// maintenance unarmed (the boot default — ADR-0076 D7).
fn rebooted(source: &Keyspace, corpus: &[(Vec<u8>, Vec<u8>)], now: Nanos) -> Keyspace {
    let mut ks = Keyspace::new(StoreConfig::default());
    ks.seed_catalog(&source.export_catalog(100, 100, 100)).expect("seed");
    for (key, idoc) in corpus {
        let full =
            RecordView::DocFull { ns: NS, key, lineage: DocLineage::FIRST, version: 1, idoc };
        ks.apply_record(&full, now, ANCHOR).expect("phase-A replay");
    }
    ks
}

fn replay_tail(ks: &mut Keyspace, tail: &[TailOp], now: Nanos) {
    for op in tail {
        let record = match op {
            TailOp::Doc(key, idoc) => {
                RecordView::DocFull { ns: NS, key, lineage: DocLineage::FIRST, version: 2, idoc }
            }
            TailOp::Del(key) => RecordView::Delete { ns: NS, key },
            TailOp::Str(key, value) => RecordView::StringPostImage { ns: NS, key, value },
        };
        ks.apply_record(&record, now, ANCHOR).expect("tail replay");
    }
}

/// A recorded phase-A write: `(key, idoc bytes)` — what the checkpoint
/// images replay as on the fresh node.
type Corpus = Vec<(Vec<u8>, Vec<u8>)>;

/// Builds the converged pre-crash node: phase-A corpus, declarations,
/// backfill to convergence, catalog flipped `ready` (the fleet-flip
/// model — sets the `was_ready` hint on the reboot).
fn converged_fixture(count: u64, seed: u64) -> (Keyspace, Corpus, Rng, Nanos) {
    let mut ks = durable_keyspace();
    let mut rng = Rng(seed);
    let now = Nanos(1_000_000_000);
    let mut corpus = Vec::new();
    for i in 0..count {
        let key = key_of(i);
        let doc = parse(&random_doc(&mut rng));
        ks.ns_store_mut(NS).unwrap().json_set(&key, &doc, Default::default(), now).expect("set");
        corpus.push((key, doc));
    }
    declare_indexes(&mut ks);
    backfill_to_convergence(&mut ks, now);
    for &(id, ..) in INDEXES {
        ks.idx_registry_mut()
            .set_catalog_state(IndexId(id), IndexState::Ready)
            .expect("fleet flip model");
    }
    (ks, corpus, rng, now)
}

// ---- leg 1 + 10 + 11: the happy path ---------------------------------------

#[test]
fn sidecar_load_and_catchup_converge_to_oracle() {
    let (mut ks1, corpus, mut rng, mut now) = converged_fixture(200, 0x5106);
    // Fuzzy emission: live mutations race the cursor between chunks;
    // each is recorded as the tail the fresh node replays.
    let mut tail: Vec<TailOp> = Vec::new();
    let (fs, path) = emit_sidecars(
        &mut ks1,
        7,
        |ks| {
            let mut inner_now = now;
            for _ in 0..2 {
                live_op(ks, &mut rng, inner_now, &mut tail);
                inner_now.0 += 1_000_000;
            }
        },
        |meta| meta,
    );
    now.0 += 1_000_000_000;

    // The fresh boot: seed, phase-A replay (maintenance unarmed), the
    // sidecar load, CatchUp arming, tail replay, commit.
    let mut ks2 = rebooted(&ks1, &corpus, now);
    for &(id, ..) in INDEXES {
        assert_eq!(
            ks2.idx_registry().was_ready(IndexId(id)),
            Some(true),
            "the ADR-0075 D4 hint survives the reboot"
        );
    }
    let mut loader = SidecarLoader::default();
    load_sections(&mut loader, &mut ks2, &fs, &path);
    loader.finish_load(&mut ks2);
    replay_tail(&mut ks2, &tail, now);
    let rows = loader.commit_ready(&mut ks2);

    assert_eq!(rows.len(), INDEXES.len());
    for row in &rows {
        assert!(
            matches!(row.decision, SidecarBootDecision::Loaded { .. }),
            "index {} must load, got {:?}",
            row.id.0,
            row.decision
        );
    }
    let info = ks2.idx_sidecar_info();
    assert_eq!((info.loaded, info.rebuilt, info.damaged_sections), (4, 0, 0));
    // Loaded trees equal the from-scratch derivation over the replayed
    // corpus — the L2 compass property, through a crash.
    for &(id, path_expr, key_type) in INDEXES {
        assert_eq!(
            tree_entries(&ks2, IndexId(id)),
            oracle_entries(&mut ks2, path_expr, key_type, now),
            "index {id} diverged from the oracle after load + catch-up"
        );
        assert_eq!(ks2.idx_registry().cell_state(IndexId(id)), Some(IndexState::Ready));
    }
    // No walk happens: the S05 machine sees cell-Ready and creates no
    // jobs (the <15 s gate's whole mechanism).
    let stats = ks2.idx_backfill_tick(now, BackfillBudget::default());
    assert_eq!((stats.active, stats.docs_scanned), (0, 0), "a loaded index never re-walks");
    // Post-commit live mutations run Strict over converged trees — any
    // load divergence fires the found/fresh debug asserts here.
    let mut tail2 = Vec::new();
    for _ in 0..64 {
        live_op(&mut ks2, &mut rng, now, &mut tail2);
        now.0 += 1_000_000;
    }
    for &(id, path_expr, key_type) in INDEXES {
        assert_eq!(
            tree_entries(&ks2, IndexId(id)),
            oracle_entries(&mut ks2, path_expr, key_type, now),
            "index {id} diverged under post-commit live load"
        );
    }
}

#[test]
fn empty_converged_tree_loads_from_its_final_section() {
    // An index on an unwritten namespace converges instantly (S05) and
    // emits exactly one zero-entry FINAL section (ADR-0078 D2).
    let mut ks1 = durable_keyspace();
    declare_indexes(&mut ks1);
    let now = Nanos(1_000_000_000);
    backfill_to_convergence(&mut ks1, now);
    let (fs, path) = emit_sidecars(&mut ks1, 7, |_| {}, |meta| meta);
    let mut ks2 = rebooted(&ks1, &[], now);
    let mut loader = SidecarLoader::default();
    load_sections(&mut loader, &mut ks2, &fs, &path);
    loader.finish_load(&mut ks2);
    let rows = loader.commit_ready(&mut ks2);
    for row in rows {
        assert_eq!(row.decision, SidecarBootDecision::Loaded { entries: 0 });
    }
    assert_eq!(ks2.idx_sidecar_info().loaded, 4);
}

// ---- legs 2–8: the discard vocabulary --------------------------------------

/// Runs a full crash cycle with `rewrite` corrupting index 1's stream
/// identity (or `sabotage` reshaping the file's emission), asserts the
/// expected per-index outcome, then proves the S05 rebuild converges
/// over the discarded tree — the discard → rebuild composition.
fn discard_leg(rewrite: impl Fn(IdxSidecarMeta) -> IdxSidecarMeta, expect: SidecarRebuildReason) {
    let (mut ks1, corpus, _rng, now) = converged_fixture(60, 42);
    let (fs, path) = emit_sidecars(
        &mut ks1,
        7,
        |_| {},
        |meta| if meta.index_id == 1 { rewrite(meta) } else { meta },
    );
    let mut ks2 = rebooted(&ks1, &corpus, now);
    let mut loader = SidecarLoader::default();
    load_sections(&mut loader, &mut ks2, &fs, &path);
    loader.finish_load(&mut ks2);
    let rows = loader.commit_ready(&mut ks2);
    let row = rows.iter().find(|r| r.id == IndexId(1)).expect("row");
    assert_eq!(row.decision, SidecarBootDecision::Rebuilt { reason: expect });
    assert!(row.was_ready, "the downgrade case: was serving before the crash");
    assert_eq!(ks2.idx_tree(NS, IndexId(1)).map(|t| t.len()), Some(0), "discard empties the tree");
    for row in rows.iter().filter(|r| r.id != IndexId(1)) {
        assert!(
            matches!(row.decision, SidecarBootDecision::Loaded { .. }),
            "one index's discard never spreads"
        );
    }
    assert_eq!(ks2.idx_sidecar_info().rebuilt, 1);
    // The discarded index rebuilds through the ordinary S05 machine.
    backfill_to_convergence(&mut ks2, now);
    let (_, path_expr, key_type) = INDEXES[0];
    assert_eq!(
        tree_entries(&ks2, IndexId(1)),
        oracle_entries(&mut ks2, path_expr, key_type, now),
        "the rebuild converges over the discarded tree"
    );
}

#[test]
fn generation_mismatch_discards_and_rebuilds() {
    discard_leg(
        |meta| IdxSidecarMeta { generation: meta.generation + 1, ..meta },
        SidecarRebuildReason::GenerationMismatch,
    );
}

#[test]
fn encoding_version_mismatch_discards_and_rebuilds() {
    discard_leg(
        |meta| IdxSidecarMeta { key_encoding_version: 2, ..meta },
        SidecarRebuildReason::EncodingVersion,
    );
}

#[test]
fn key_scheme_mismatch_discards_and_rebuilds() {
    // Index 1 is F64 (Fixed8); stamping VarKey flips the declared
    // scheme. The keys themselves are 8 bytes either way — only the
    // meta lies, which is exactly what the check exists for. The
    // entries re-encode as var (2-byte length prefixes), so the file
    // stays canonical and only the loader's scheme check can object.
    discard_leg(
        |meta| IdxSidecarMeta { fixed8: false, ..meta },
        SidecarRebuildReason::SchemeMismatch,
    );
}

#[test]
fn ordinal_gap_discards_and_rebuilds() {
    // A hole in the ordinal chain across sections — the shape an
    // unattributed damaged middle section leaves behind (ADR-0078 D4).
    let (ks1, corpus, _rng, now) = converged_fixture(60, 43);
    let fs = MemFs::new();
    let dir = Path::new("/ckpt");
    fs.create_dir_all(dir).unwrap();
    let mut w = SyncIckWriter::create_v2(
        fs.clone(),
        dir,
        &CkptConfig { section_bytes: 64, ..Default::default() },
        0,
        1,
        Lsn::new(SegmentId(1), 64),
        &[NS.0],
    )
    .expect("create v2");
    let meta = IdxSidecarMeta {
        ns: NS.0,
        index_id: 3, // $.qty — I64, Fixed8: 16-byte entries, 64-byte sections
        generation: 3,
        key_encoding_version: INDEX_KEY_ENCODING_VERSION,
        fixed8: true,
    };
    let mut pairs: Vec<(Vec<u8>, u64)> = Vec::new();
    let mut cursor = OrderedCursor::from_start();
    ks1.idx_sidecar_emit(NS, IndexId(3), &mut cursor, u32::MAX, |k, r| {
        pairs.push((k.to_vec(), r));
    });
    assert!(pairs.len() >= 12, "fixture large enough to span sections");
    // Emit 0..8 (two full 4-entry sections), then skip two ordinals.
    for (ordinal, (key, entry_ref)) in pairs.iter().take(8).enumerate() {
        w.append_idx_entry(&meta, ordinal as u64, key, *entry_ref).expect("entry");
    }
    for (at, (key, entry_ref)) in pairs.iter().enumerate().skip(10) {
        w.append_idx_entry(&meta, at as u64, key, *entry_ref).expect("entry");
    }
    w.append_idx_final(&meta, pairs.len() as u64).expect("final");
    w.finish().expect("finish");
    let path = dir.join(ick_file_name(1));

    let mut ks2 = rebooted(&ks1, &corpus, now);
    let mut loader = SidecarLoader::default();
    load_sections(&mut loader, &mut ks2, &fs, &path);
    loader.finish_load(&mut ks2);
    let rows = loader.commit_ready(&mut ks2);
    let row = rows.iter().find(|r| r.id == IndexId(3)).expect("row");
    assert_eq!(
        row.decision,
        SidecarBootDecision::Rebuilt { reason: SidecarRebuildReason::NonContiguous }
    );
    backfill_to_convergence(&mut ks2, now);
    let (_, path_expr, key_type) = INDEXES[2];
    assert_eq!(tree_entries(&ks2, IndexId(3)), oracle_entries(&mut ks2, path_expr, key_type, now));
}

#[test]
fn cross_section_regression_discards_as_out_of_order() {
    // Sections individually canonical but mutually regressed — only the
    // loader's tree-maximum check (the append refusal) can see it.
    let (ks1, corpus, _rng, now) = converged_fixture(60, 44);
    let fs = MemFs::new();
    let dir = Path::new("/ckpt");
    fs.create_dir_all(dir).unwrap();
    let mut w = SyncIckWriter::create_v2(
        fs.clone(),
        dir,
        &CkptConfig { section_bytes: 64, ..Default::default() },
        0,
        1,
        Lsn::new(SegmentId(1), 64),
        &[NS.0],
    )
    .expect("create v2");
    let meta = IdxSidecarMeta {
        ns: NS.0,
        index_id: 3,
        generation: 3,
        key_encoding_version: INDEX_KEY_ENCODING_VERSION,
        fixed8: true,
    };
    let mut pairs: Vec<(Vec<u8>, u64)> = Vec::new();
    let mut cursor = OrderedCursor::from_start();
    ks1.idx_sidecar_emit(NS, IndexId(3), &mut cursor, u32::MAX, |k, r| {
        pairs.push((k.to_vec(), r));
    });
    // Sections of 4; swap the second section's pairs with the first —
    // ordinals stay contiguous, keys regress at the boundary.
    let mut reordered = pairs.clone();
    reordered.swap(0, 4);
    reordered.swap(1, 5);
    reordered.swap(2, 6);
    reordered.swap(3, 7);
    for (ordinal, (key, entry_ref)) in reordered.iter().enumerate() {
        // In-section ascension must hold for the writer; sections are 4
        // entries, and each swapped block is internally ascending.
        w.append_idx_entry(&meta, ordinal as u64, key, *entry_ref).expect("entry");
    }
    w.append_idx_final(&meta, reordered.len() as u64).expect("final");
    w.finish().expect("finish");
    let path = dir.join(ick_file_name(1));

    let mut ks2 = rebooted(&ks1, &corpus, now);
    let mut loader = SidecarLoader::default();
    load_sections(&mut loader, &mut ks2, &fs, &path);
    loader.finish_load(&mut ks2);
    let rows = loader.commit_ready(&mut ks2);
    let row = rows.iter().find(|r| r.id == IndexId(3)).expect("row");
    assert_eq!(
        row.decision,
        SidecarBootDecision::Rebuilt { reason: SidecarRebuildReason::OutOfOrder }
    );
}

#[test]
fn missing_final_discards_as_incomplete() {
    // The abandoned-mid-emission shape (a drop/rebuild/degrade racing
    // the walk — ADR-0078 D1): entries, no FINAL.
    let (ks1, corpus, _rng, now) = converged_fixture(60, 45);
    let fs = MemFs::new();
    let dir = Path::new("/ckpt");
    fs.create_dir_all(dir).unwrap();
    let mut w = SyncIckWriter::create_v2(
        fs.clone(),
        dir,
        &CkptConfig { section_bytes: 256, ..Default::default() },
        0,
        1,
        Lsn::new(SegmentId(1), 64),
        &[NS.0],
    )
    .expect("create v2");
    let meta = IdxSidecarMeta {
        ns: NS.0,
        index_id: 3,
        generation: 3,
        key_encoding_version: INDEX_KEY_ENCODING_VERSION,
        fixed8: true,
    };
    let mut cursor = OrderedCursor::from_start();
    let mut pairs = Vec::new();
    ks1.idx_sidecar_emit(NS, IndexId(3), &mut cursor, 8, |k, r| pairs.push((k.to_vec(), r)));
    for (ordinal, (key, entry_ref)) in pairs.iter().enumerate() {
        w.append_idx_entry(&meta, ordinal as u64, key, *entry_ref).expect("entry");
    }
    w.finish().expect("finish — seals the FINAL-less tail section");
    let path = dir.join(ick_file_name(1));

    let mut ks2 = rebooted(&ks1, &corpus, now);
    let mut loader = SidecarLoader::default();
    load_sections(&mut loader, &mut ks2, &fs, &path);
    loader.finish_load(&mut ks2);
    let rows = loader.commit_ready(&mut ks2);
    let row = rows.iter().find(|r| r.id == IndexId(3)).expect("row");
    assert_eq!(
        row.decision,
        SidecarBootDecision::Rebuilt { reason: SidecarRebuildReason::Incomplete }
    );
    assert_eq!(ks2.idx_tree(NS, IndexId(3)).map(|t| t.len()), Some(0));
}

#[test]
fn sections_after_final_discard_as_after_final() {
    let (ks1, corpus, _rng, now) = converged_fixture(60, 46);
    let fs = MemFs::new();
    let dir = Path::new("/ckpt");
    fs.create_dir_all(dir).unwrap();
    let mut w = SyncIckWriter::create_v2(
        fs.clone(),
        dir,
        &CkptConfig::default(),
        0,
        1,
        Lsn::new(SegmentId(1), 64),
        &[NS.0],
    )
    .expect("create v2");
    let meta = IdxSidecarMeta {
        ns: NS.0,
        index_id: 3,
        generation: 3,
        key_encoding_version: INDEX_KEY_ENCODING_VERSION,
        fixed8: true,
    };
    let mut cursor = OrderedCursor::from_start();
    let mut pairs = Vec::new();
    ks1.idx_sidecar_emit(NS, IndexId(3), &mut cursor, 8, |k, r| pairs.push((k.to_vec(), r)));
    for (ordinal, (key, entry_ref)) in pairs.iter().take(4).enumerate() {
        w.append_idx_entry(&meta, ordinal as u64, key, *entry_ref).expect("entry");
    }
    w.append_idx_final(&meta, 4).expect("final — seals");
    // A fresh section for the same index after its FINAL.
    for (ordinal, (key, entry_ref)) in pairs.iter().enumerate().skip(4) {
        w.append_idx_entry(&meta, ordinal as u64, key, *entry_ref).expect("entry");
    }
    w.append_idx_final(&meta, pairs.len() as u64).expect("second final");
    w.finish().expect("finish");
    let path = dir.join(ick_file_name(1));

    let mut ks2 = rebooted(&ks1, &corpus, now);
    let mut loader = SidecarLoader::default();
    load_sections(&mut loader, &mut ks2, &fs, &path);
    loader.finish_load(&mut ks2);
    let rows = loader.commit_ready(&mut ks2);
    let row = rows.iter().find(|r| r.id == IndexId(3)).expect("row");
    assert_eq!(
        row.decision,
        SidecarBootDecision::Rebuilt { reason: SidecarRebuildReason::AfterFinal }
    );
}

// ---- legs 9 + 12: stale declarations and damage accounting ------------------

#[test]
fn stale_declaration_sections_are_swallowed() {
    let (mut ks1, corpus, _rng, now) = converged_fixture(60, 47);
    // Emit with index 1's id rewritten to one the catalog never held.
    let (fs, path) = emit_sidecars(
        &mut ks1,
        7,
        |_| {},
        |meta| {
            if meta.index_id == 1 { IdxSidecarMeta { index_id: 63, ..meta } } else { meta }
        },
    );
    let mut ks2 = rebooted(&ks1, &corpus, now);
    let mut loader = SidecarLoader::default();
    load_sections(&mut loader, &mut ks2, &fs, &path);
    loader.finish_load(&mut ks2);
    let rows = loader.commit_ready(&mut ks2);
    // Index 1 has no stream (its sections went to the unknown id) —
    // honest NoSidecar; everything else loads; nothing crashes.
    let row = rows.iter().find(|r| r.id == IndexId(1)).expect("row");
    assert_eq!(
        row.decision,
        SidecarBootDecision::Rebuilt { reason: SidecarRebuildReason::NoSidecar }
    );
    assert_eq!(ks2.idx_sidecar_info().loaded, 3);
}

#[test]
fn damaged_sections_count_into_the_info_fold() {
    let (mut ks1, corpus, _rng, now) = converged_fixture(30, 48);
    let (fs, path) = emit_sidecars(&mut ks1, 7, |_| {}, |meta| meta);
    let mut ks2 = rebooted(&ks1, &corpus, now);
    let mut loader = SidecarLoader::default();
    loader.note_damaged();
    loader.note_damaged();
    load_sections(&mut loader, &mut ks2, &fs, &path);
    loader.finish_load(&mut ks2);
    let _ = loader.commit_ready(&mut ks2);
    assert_eq!(ks2.idx_sidecar_info().damaged_sections, 2);
}
