//! `m45-backfill` (M4.5-S05, ADR-0077): crash-at-every-backfill-phase
//! under the deterministic full-node driver — the S05 AC 3 scenario.
//!
//! One seeded life: boot → durable-ns DDL → a document corpus → an
//! index-bearing catalog persisted through the production `META` swap →
//! then one power cut **at each backfill phase** (before the walk, mid-
//! walk, after a cell reported ready, after the fleet flipped), each
//! followed by a reboot that must regress every index to `backfilling`
//! (ADR-0075 D4), refuse bindings typed while rebuilding, restart the
//! walk from zero (ADR-0077 D2 — no watermark survives), and re-converge.
//! Foreground mutations race the mid-walk leg through the real plane
//! bracket.
//!
//! The oracle is the S05 "ready only with verified contents" rule made
//! executable: the **first moment** any cell's catalog entry reads
//! `ready`, that cell's tree must equal the from-scratch derivation off
//! its own recovered documents (the read-only digest walk — independent
//! of the maintenance/backfill code), and the final boot verifies every
//! cell × index the same way. `--verify-determinism` runs the scenario
//! twice and requires `trace_hash` identity (L7).

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::rc::Rc;

use inf_doc::path::{EvalLimits, compile, eval, resolve};
use inf_doc::{DocValue, PathProgram, TapeDoc};
use inf_foundation::rng::{Entropy, SplitMix64};
use inf_foundation::time::{Clock, Nanos, VirtualClock};
use inf_store::{
    CellStore, CheckpointImage, IndexId, IndexKeyBuf, IndexKeyType, IndexScalar, IndexSpec,
    IndexState, Keyspace, NsId, OrderedCursor, index_key_encode,
};

use crate::durable::{
    DurableScenario, DurableWorkload, MiniClient, Node, TraceObserver, boot, build_disk,
};
use crate::net::Plant;

/// The two declarations under test: a numeric chain and a string chain
/// (the `[*]` multi-match class is store-level-proven in
/// `index_backfill_storm`; the sim exercises the machine, not the eval).
const INDEXES: &[(u32, &str, IndexKeyType)] =
    &[(1, "$.price", IndexKeyType::F64), (2, "$.tag", IndexKeyType::Utf8)];

/// Scenario knobs — the DSL v0 shape (a struct, not a language).
#[derive(Debug)]
pub struct BackfillScenario {
    pub seed: u64,
    pub cells: u16,
    /// Corpus documents written before the declarations persist.
    pub docs: u64,
    /// Max virtual nanoseconds per scheduler step.
    pub step_ns_max: u64,
}

impl BackfillScenario {
    #[must_use]
    pub fn m45_backfill(seed: u64) -> BackfillScenario {
        // The corpus is sized so the walk spans many MAINTAIN ticks —
        // the mid-walk cut must land inside a genuinely open window
        // (the cut record discloses the scanned count it landed on).
        BackfillScenario { seed, cells: 2, docs: 4000, step_ns_max: 2_000_000 }
    }
}

#[derive(Debug, Default)]
pub struct BackfillReport {
    pub violations: Vec<String>,
    /// Boots driven (setup + one per phase cut).
    pub boots: u64,
    /// Power cuts in order, each with the walk progress it landed on.
    pub cuts: Vec<String>,
    /// Ready-implies-verified oracle checks that ran.
    pub ready_checks: u64,
    /// Typed binding refusals observed while a rebuild was in progress —
    /// zero at run end is itself a violation (the window went untested).
    pub refused_bindings: u64,
    /// Mid-walk foreground mutations that raced the walk.
    pub raced_mutations: u64,
    pub scheduler_steps: u64,
    pub trace_hash: u64,
}

impl BackfillReport {
    #[must_use]
    pub fn ok(&self) -> bool {
        self.violations.is_empty()
    }
}

/// Boot parameters: no writers (traffic is scenario-driven), no stall
/// device (instant fsyncs keep the phase windows deterministic and the
/// run fast), checkpoints effectively off (index sidecars are S06 — this
/// scenario must exercise the rebuild-from-log path).
fn base(scenario: &BackfillScenario) -> DurableScenario {
    DurableScenario {
        seed: scenario.seed,
        workload: DurableWorkload::KeyValue,
        cells: scenario.cells,
        always_writers: 0,
        esec_writers: 0,
        mem_writers: 0,
        ops_per_writer: 0,
        keys_per_writer: 0,
        value_max: 0,
        step_ns_max: scenario.step_ns_max,
        double_cut: false,
        plant: Plant::None,
        segment_bytes: 64 << 10,
        ckpt_interval_bytes: 1 << 30,
        ckpt_stream_bytes_per_sec: None,
        stall: None,
        replay_canary: false,
        io_mode: inf_server::SegmentIoMode::Buffered,
    }
}

fn key_of(i: u64) -> Vec<u8> {
    format!("d:{i:05}").into_bytes()
}

fn doc_of(i: u64) -> Vec<u8> {
    // i64/f64 coercion diversity on `price` (the one-truth-table rule is
    // S02-proven; here it just keeps the corpus honest).
    let price = if i.is_multiple_of(3) { format!("{}", i % 97) } else { format!("{}.5", i % 97) };
    format!(r#"{{"price":{price},"tag":"t{}"}}"#, i % 17).into_bytes()
}

/// From-scratch truth for one `(cell, index)`: the read-only digest walk
/// over the cell's recovered documents, evaluated + encoded through
/// public APIs — independent of the hook and the walk (shared with the
/// S06 sidecar scenario).
pub(crate) fn cell_truth(
    ks: &Keyspace,
    ns: NsId,
    program: &PathProgram,
    key_type: IndexKeyType,
    now: Nanos,
) -> BTreeSet<(Vec<u8>, u64)> {
    let mut out = BTreeSet::new();
    let Some(store) = ks.ns_store(ns) else { return out };
    let mut buf = IndexKeyBuf::new();
    let mut cursor = 0u64;
    loop {
        cursor = store.digest_checkpoint_images(cursor, 128, now, |key, image, _expire| {
            let CheckpointImage::JsonDoc { idoc, .. } = image else { return };
            let tape = TapeDoc::from_validated_bytes(idoc);
            let root = DocValue::from(tape.root());
            let Ok(matches) = eval(program, root, &EvalLimits::default()) else { return };
            let hash = CellStore::hash_key(key);
            for steps in matches.iter() {
                let Some(value) = resolve(root, steps) else { continue };
                let scalar = match value {
                    DocValue::Null => IndexScalar::Null,
                    DocValue::Bool(b) => IndexScalar::Bool(b),
                    DocValue::I64(v) => IndexScalar::I64(v),
                    DocValue::F64(f) => IndexScalar::F64(f),
                    DocValue::Str(s) => IndexScalar::Utf8(s.to_str()),
                    _ => continue,
                };
                if index_key_encode(key_type, scalar, &mut buf).is_ok() {
                    out.insert((buf.as_bytes().to_vec(), hash));
                }
            }
        });
        if cursor == 0 {
            return out;
        }
    }
}

pub(crate) fn cell_tree(ks: &Keyspace, ns: NsId, id: IndexId) -> BTreeSet<(Vec<u8>, u64)> {
    let mut out = BTreeSet::new();
    let Some(tree) = ks.idx_tree(ns, id) else { return out };
    let mut cursor = OrderedCursor::from_start();
    while let Some((key, entry_ref)) = tree.cursor_next(&mut cursor) {
        out.insert((key.to_vec(), entry_ref));
    }
    out
}

/// The ready-implies-verified oracle for one cell × index (`spec` is an
/// `INDEXES` row).
fn verify_cell_index(
    node: &Node,
    cell: usize,
    ns: NsId,
    spec: (u32, &str, IndexKeyType),
    now: Nanos,
    context: &str,
    report: &mut BackfillReport,
) {
    let (id, path, key_type) = spec;
    let id = IndexId(id);
    let program = compile(path.as_bytes()).expect("valid index path");
    let ks = node.plane(cell).keyspace();
    let truth = cell_truth(&ks, ns, &program, key_type, now);
    let tree = cell_tree(&ks, ns, id);
    report.ready_checks += 1;
    if tree != truth {
        report.violations.push(format!(
            "{context}: cell {cell} index {} tree ≠ scan-derived truth \
             ({} tree entries vs {} derived)",
            id.0,
            tree.len(),
            truth.len()
        ));
    }
    if ks.idx_degraded(ns, id) == Some(true) {
        report.violations.push(format!("{context}: cell {cell} index {} degraded", id.0));
    }
}

/// Catalog state of `(cell, id)` as this cell's registry sees it.
fn catalog_state(node: &Node, cell: usize, id: IndexId) -> Option<IndexState> {
    node.plane(cell).keyspace().idx_registry().get_by_id(id).map(|s| s.state)
}

fn fleet_catalog_ready(node: &Node, cells: u16) -> bool {
    (0..usize::from(cells)).all(|cell| {
        INDEXES
            .iter()
            .all(|&(id, ..)| catalog_state(node, cell, IndexId(id)) == Some(IndexState::Ready))
    })
}

fn any_cell_machine_ready(node: &Node, cells: u16) -> bool {
    (0..usize::from(cells)).any(|cell| {
        let ks = node.plane(cell).keyspace();
        INDEXES
            .iter()
            .any(|&(id, ..)| ks.idx_registry().cell_state(IndexId(id)) == Some(IndexState::Ready))
    })
}

fn any_docs_scanned(node: &Node, cells: u16) -> bool {
    (0..usize::from(cells))
        .any(|cell| node.plane(cell).keyspace().idx_backfill_info().docs_scanned_total > 0)
}

/// True while at least one cell still has a walking job — the open
/// mid-walk window the phase-1 cut must land inside.
fn any_walk_open(node: &Node, cells: u16) -> bool {
    (0..usize::from(cells)).any(|cell| node.plane(cell).keyspace().idx_backfill_info().walking > 0)
}

/// The post-reboot serving contract (ADR-0075 D4 + the S05 AC): the
/// declarations survived, no state reads `declared` (the seed regresses
/// through `backfilling`), and — the load-bearing half — **a binding
/// that validates implies oracle-verified contents**, while a rebuild in
/// progress refuses typed (counted; the run must observe at least one
/// refusal, or the regression window was never exercised). "Still
/// backfilling at this instant" is deliberately *not* asserted: a fast
/// cell may have re-converged before the slowest cell's recovery
/// finished — completion is legal at any point, unverified serving never.
fn check_serving_contract(
    node: &Node,
    cells: u16,
    ns: NsId,
    now: Nanos,
    context: &str,
    report: &mut BackfillReport,
) {
    let mut verify: Vec<(usize, (u32, &'static str, IndexKeyType))> = Vec::new();
    for cell in 0..usize::from(cells) {
        let ks = node.plane(cell).keyspace();
        for &(id, path, key_type) in INDEXES {
            let idx = IndexId(id);
            match ks.idx_registry().get_by_id(idx) {
                None => {
                    report.violations.push(format!("{context}: cell {cell} lost declaration {id}"))
                }
                Some(spec) => {
                    if matches!(spec.state, IndexState::Declared | IndexState::Dropping) {
                        report.violations.push(format!(
                            "{context}: cell {cell} index {id} seeded {:?}",
                            spec.state
                        ));
                    }
                    if ks.idx_registry().validate_binding(ns, idx, spec.generation).is_ok() {
                        verify.push((cell, (id, path, key_type)));
                    } else {
                        report.refused_bindings += 1;
                    }
                }
            }
        }
    }
    for (cell, spec) in verify {
        verify_cell_index(node, cell, ns, spec, now, context, report);
    }
}

/// Runs one seeded scenario. Structure: setup boot (DDL + corpus +
/// catalog persist) → one cut per phase, each reboot re-checked for the
/// regression contract → a final boot driven to fleet-ready and fully
/// oracle-verified.
pub fn run_backfill_scenario(scenario: &BackfillScenario) -> BackfillReport {
    let clock = Rc::new(VirtualClock::new(Nanos(1)));
    let disk = build_disk(scenario.seed, None);
    let observer = TraceObserver::default();
    let mut rng = SplitMix64::new(scenario.seed ^ 0xBACF_1115);
    let mut report = BackfillReport::default();
    let mut trace: Vec<u8> = scenario.seed.to_le_bytes().to_vec();
    let base = base(scenario);
    let dir = PathBuf::from("node");

    // Bounded drive: steps until `pred`, a stall verdict past the cap.
    macro_rules! drive_until {
        ($node:expr, $what:expr, $pred:expr) => {{
            let mut done = false;
            for _ in 0..400_000u64 {
                if $pred {
                    done = true;
                    break;
                }
                if let Err(err) = $node.step(&mut rng, &clock, &disk, scenario.step_ns_max) {
                    report.violations.push(format!("{}: step failed: {err}", $what));
                    break;
                }
                report.scheduler_steps += 1;
            }
            if !done {
                report.violations.push(format!("{}: stalled before the predicate", $what));
            }
            done
        }};
    }

    macro_rules! reboot {
        ($what:expr) => {{
            report.boots += 1;
            match boot(&base, dir.clone(), &disk, &clock, &observer) {
                Ok(node) => node,
                Err(err) => {
                    report.violations.push(format!("{}: boot failed: {err}", $what));
                    report.trace_hash = inf_foundation::hash64(&trace, 0x4501_BACF);
                    return report;
                }
            }
        }};
    }

    // ---- setup boot: DDL + corpus + the index-bearing catalog ---------
    let mut node = reboot!("setup");
    if !drive_until!(node, "setup recovery", node.ready()) {
        report.trace_hash = inf_foundation::hash64(&trace, 0x4501_BACF);
        return report;
    }
    let mut client = MiniClient::connect(&mut node, 0);
    let mut call = |node: &mut Node,
                    rng: &mut SplitMix64,
                    report: &mut BackfillReport,
                    argv: &[&[u8]],
                    what: &str|
     -> bool {
        match client.call(node, rng, &clock, &disk, scenario.step_ns_max, argv) {
            Ok(Some(reply)) if reply.first() != Some(&b'-') => true,
            Ok(reply) => {
                report.violations.push(format!("{what}: answered {reply:?}"));
                false
            }
            Err(err) => {
                report.violations.push(format!("{what}: {err}"));
                false
            }
        }
    };
    let created =
        call(
            &mut node,
            &mut rng,
            &mut report,
            &[b"INF.NS", b"CREATE", b"bf", b"MODE", b"durable", b"FSYNC", b"always"],
            "ns create",
        ) && call(&mut node, &mut rng, &mut report, &[b"INF.NS", b"USE", b"bf"], "ns use");
    if created {
        for i in 0..scenario.docs {
            let key = key_of(i);
            let doc = doc_of(i);
            if !call(
                &mut node,
                &mut rng,
                &mut report,
                &[b"JSON.SET", &key, b"$", &doc],
                "corpus write",
            ) {
                break;
            }
        }
    }
    let Some(ns) = node.plane(0).keyspace().ns_iter().find(|s| s.name == b"bf").map(|s| s.id)
    else {
        report.violations.push("setup: namespace bf missing after DDL".into());
        report.trace_hash = inf_foundation::hash64(&trace, 0x4501_BACF);
        return report;
    };
    // Declarations enter through the production persistence path (the
    // catalog `META` swap); the DDL fan is S10's — boot-seeding is
    // exactly the ADR-0075 D4 path this scenario must exercise. One
    // pre-crash-`ready` entry rides along to cover the D4 hint class.
    let mut catalog = node.plane(0).keyspace().export_catalog(node.control.next_ns_id(), 3, 3);
    for &(id, path, key_type) in INDEXES {
        let program = compile(path.as_bytes()).expect("valid index path").as_bytes().to_vec();
        catalog.index.entries.push(IndexSpec {
            id: IndexId(id),
            generation: u64::from(id),
            ns,
            name: format!("by-{id}").into_bytes(),
            program,
            key_type,
            state: if id == 2 { IndexState::Ready } else { IndexState::Declared },
        });
    }
    node.control.request_persist(catalog);
    for _ in 0..64 {
        if node.step(&mut rng, &clock, &disk, scenario.step_ns_max).is_err() {
            report.violations.push("setup: persist drain failed".into());
        }
        report.scheduler_steps += 1;
    }
    trace.extend_from_slice(b"setup-done");

    // ---- one cut per phase --------------------------------------------
    // Each iteration drives the live node **to** its phase, cuts there
    // (the scanned count at the cut is recorded — phase realism is an
    // artifact, not an assumption, L10), then reboots and re-checks the
    // serving contract. Phase 0 is the setup state itself (declarations
    // persisted, zero walk progress — the live registries never saw them:
    // the DDL fan is S10's, boot-seeding is the path under test). The
    // last iteration's reboot is the final life, driven to fleet-ready
    // and fully verified below.
    for phase in 0..4usize {
        let what: &'static str =
            ["before-backfill", "mid-walk", "cell-ready", "fleet-ready"][phase];
        match phase {
            0 => {}
            1 => {
                // Cut the instant an open walk shows progress — the cut
                // itself is the mid-walk event (the record discloses the
                // scanned count it landed on).
                drive_until!(
                    node,
                    what,
                    any_docs_scanned(&node, scenario.cells) && any_walk_open(&node, scenario.cells)
                );
            }
            2 => {
                // One cell ready while the other still walks — the wide
                // window; foreground mutations race the open walk here
                // through the real plane bracket.
                drive_until!(node, what, any_cell_machine_ready(&node, scenario.cells));
                let mut writer = MiniClient::connect(&mut node, 0);
                let _ = writer.call(
                    &mut node,
                    &mut rng,
                    &clock,
                    &disk,
                    scenario.step_ns_max,
                    &[b"INF.NS", b"USE", b"bf"],
                );
                for round in 0..40u64 {
                    if !any_walk_open(&node, scenario.cells) {
                        break;
                    }
                    let i = rng.next_u64() % scenario.docs;
                    let key = key_of(i);
                    let outcome = if round % 5 == 4 {
                        writer.call(
                            &mut node,
                            &mut rng,
                            &clock,
                            &disk,
                            scenario.step_ns_max,
                            &[b"DEL", &key],
                        )
                    } else {
                        let doc = doc_of(i ^ round);
                        writer.call(
                            &mut node,
                            &mut rng,
                            &clock,
                            &disk,
                            scenario.step_ns_max,
                            &[b"JSON.SET", &key, b"$", &doc],
                        )
                    };
                    match outcome {
                        Ok(Some(_)) => report.raced_mutations += 1,
                        _ => break,
                    }
                }
            }
            _ => {
                if drive_until!(node, what, fleet_catalog_ready(&node, scenario.cells)) {
                    // Ready implies verified — the whole fleet, pre-cut.
                    let now = clock.now();
                    for cell in 0..usize::from(scenario.cells) {
                        for &spec in INDEXES {
                            verify_cell_index(
                                &node,
                                cell,
                                ns,
                                spec,
                                now,
                                "fleet-ready (pre-cut)",
                                &mut report,
                            );
                        }
                    }
                }
            }
        }
        let scanned: u64 = (0..usize::from(scenario.cells))
            .map(|c| node.plane(c).keyspace().idx_backfill_info().docs_scanned_total)
            .sum();
        disk.power_cut(scenario.seed ^ (0xC0DE_0500 + phase as u64));
        report.cuts.push(format!("{what}(scanned={scanned})"));
        trace.extend_from_slice(what.as_bytes());
        trace.extend_from_slice(&scanned.to_le_bytes());
        node = reboot!(what);
        if !drive_until!(node, what, node.ready()) {
            break;
        }
        check_serving_contract(&node, scenario.cells, ns, clock.now(), what, &mut report);
    }

    // ---- the final life (post-fleet-ready cut): everything rebuilds ----
    if drive_until!(node, "final backfill", fleet_catalog_ready(&node, scenario.cells)) {
        let now = clock.now();
        for cell in 0..usize::from(scenario.cells) {
            {
                let ks = node.plane(cell).keyspace();
                for &(id, ..) in INDEXES {
                    let id = IndexId(id);
                    let generation = ks.idx_registry().get_by_id(id).map_or(0, |s| s.generation);
                    if ks.idx_registry().validate_binding(ns, id, generation).is_err() {
                        report.violations.push(format!(
                            "final: cell {cell} index {} refuses a ready binding",
                            id.0
                        ));
                    }
                }
            }
            for &spec in INDEXES {
                verify_cell_index(&node, cell, ns, spec, now, "final", &mut report);
            }
        }
        for cell in 0..usize::from(scenario.cells) {
            let ks = node.plane(cell).keyspace();
            for &(id, ..) in INDEXES {
                let entries = cell_tree(&ks, ns, IndexId(id)).len() as u64;
                trace.extend_from_slice(&entries.to_le_bytes());
            }
        }
    }
    if report.refused_bindings == 0 {
        report.violations.push(
            "no rebuild-window binding refusal was ever observed — the regression \
             path went unexercised"
                .into(),
        );
    }

    trace.extend_from_slice(&report.boots.to_le_bytes());
    trace.extend_from_slice(&report.ready_checks.to_le_bytes());
    trace.extend_from_slice(&(report.violations.len() as u64).to_le_bytes());
    report.trace_hash = inf_foundation::hash64(&trace, 0x4501_BACF);
    report
}
