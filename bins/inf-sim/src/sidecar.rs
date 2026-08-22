//! `m45-sidecar` (M4.5-S06, ADR-0078): the sidecar-mid-storm scenario —
//! checkpoint under a mutation storm, power cut, boot with sidecar load
//! and tail catch-up, equivalence oracle green (the S06 AC 2), plus the
//! crash-matrix row's shape (cut mid-checkpoint-write ⇒ nothing was
//! published, boot rebuilds — AC 3).
//!
//! One seeded life: boot → durable-ns DDL → corpus → index catalog
//! persisted through the production `META` swap → backfill converges
//! (the S05 machine, live) → then two cuts:
//!
//! 1. **mid-write**: `INF.CKPT` requested, the cut lands while the
//!    stream is open and nothing was published — the reboot has no
//!    manifest, loads no sidecar (`idx_sidecar_loaded == 0`, decisions
//!    read `rebuilt`), refuses bindings typed while the S05 rebuild
//!    runs, and re-converges (the checkpoint-stays-valid row: the valid
//!    recovery unit is simply the log).
//! 2. **post-publish under storm**: a second checkpoint streams while
//!    foreground mutations race it (the fuzzy-overlap window — the
//!    storm count during the open stream is disclosed and must be
//!    nonzero), publishes, more tail mutations land, then the cut. The
//!    reboot must **load** (`idx_sidecar_loaded == 2` per cell — the
//!    fast path actually ran, not luck), walk nothing
//!    (`idx_backfill_scanned == 0`), and every servable binding passes
//!    the read-only digest-walk oracle — the loaded-plus-caught-up tree
//!    equals the from-scratch derivation over the recovered documents.
//!
//! The final life then takes live mutations over the loaded trees (the
//! `Strict` maintenance path over converged indexes) and re-verifies.
//! `--verify-determinism` runs the scenario twice and requires
//! `trace_hash` identity (L7).

use std::path::PathBuf;
use std::rc::Rc;

use inf_doc::path::compile;
use inf_foundation::rng::{Entropy, SplitMix64};
use inf_foundation::time::{Clock, Nanos, VirtualClock};
use inf_store::{IndexId, IndexKeyType, IndexSpec, IndexState, NsId};

use crate::backfill::{cell_tree, cell_truth};
use crate::durable::{
    DurableScenario, DurableWorkload, MiniClient, Node, TraceObserver, boot, build_disk,
};
use crate::net::Plant;

/// The declarations under test — one per key scheme, so both sidecar
/// entry encodings ride the file (ADR-0078 D2).
const INDEXES: &[(u32, &str, IndexKeyType)] =
    &[(1, "$.price", IndexKeyType::F64), (2, "$.tag", IndexKeyType::Utf8)];

/// Scenario knobs — the DSL v0 shape.
#[derive(Debug)]
pub struct SidecarScenario {
    pub seed: u64,
    pub cells: u16,
    pub docs: u64,
    pub step_ns_max: u64,
}

impl SidecarScenario {
    #[must_use]
    pub fn m45_sidecar(seed: u64) -> SidecarScenario {
        // The corpus sizes the sidecar share of the checkpoint high
        // enough (~40% of file bytes) that seeded mid-write cuts land
        // inside the sidecar phase across the seed sweep.
        SidecarScenario { seed, cells: 2, docs: 3000, step_ns_max: 2_000_000 }
    }
}

#[derive(Debug, Default)]
pub struct SidecarReport {
    pub violations: Vec<String>,
    pub boots: u64,
    /// Cuts in order, each with its disclosed landing state.
    pub cuts: Vec<String>,
    /// Ready-implies-verified oracle checks that ran.
    pub ready_checks: u64,
    /// Typed binding refusals observed during the rebuild leg.
    pub refused_bindings: u64,
    /// Foreground mutations that landed while checkpoint 2's stream was
    /// open — the fuzzy-overlap disclosure (zero fails the run).
    pub storm_during_stream: u64,
    /// Indexes loaded from sidecars across cells on the load leg.
    pub loaded: u64,
    pub scheduler_steps: u64,
    pub trace_hash: u64,
}

impl SidecarReport {
    #[must_use]
    pub fn ok(&self) -> bool {
        self.violations.is_empty()
    }
}

/// Boot parameters: automatic checkpoints off (`INF.CKPT` drives them at
/// scenario-chosen instants), no stall device, scenario-driven traffic.
fn base(scenario: &SidecarScenario) -> DurableScenario {
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
        // Slow the stream (~2 MiB/s of virtual time) so a checkpoint
        // spans many scheduler steps — the storm has a real window to
        // land inside, and mid-write cuts land across every phase.
        ckpt_stream_bytes_per_sec: Some(2 << 20),
        ckpt_section_bytes: None,
        stall: None,
        replay_canary: false,
        io_mode: inf_server::SegmentIoMode::Buffered,
        frames_in_flight: 1,
        device: Default::default(),
        budget_oracle: false,
        reorder_oracle: false,
        ckpt_direct_refused_after: None,
        prelude: None,
    }
}

fn key_of(i: u64) -> Vec<u8> {
    format!("d:{i:05}").into_bytes()
}

fn doc_of(i: u64) -> Vec<u8> {
    let price = if i.is_multiple_of(3) { format!("{}", i % 97) } else { format!("{}.5", i % 97) };
    format!(r#"{{"price":{price},"tag":"t{}"}}"#, i % 17).into_bytes()
}

fn fleet_catalog_ready(node: &Node, cells: u16) -> bool {
    (0..usize::from(cells)).all(|cell| {
        let ks = node.plane(cell).keyspace();
        INDEXES.iter().all(|&(id, ..)| {
            ks.idx_registry().get_by_id(IndexId(id)).map(|s| s.state) == Some(IndexState::Ready)
        })
    })
}

/// Sum of committed MANIFEST publications across cells.
fn manifests_published(node: &Node, cells: u16) -> u64 {
    (0..usize::from(cells))
        .map(|cell| node.plane(cell).manifest_stats().map_or(0, |s| s.published))
        .sum()
}

/// True while any cell's checkpoint stream is open.
fn any_ckpt_streaming(node: &Node, cells: u16) -> bool {
    (0..usize::from(cells))
        .any(|cell| node.plane(cell).durable_stats().is_some_and(|s| s.ckpt_in_progress == 1))
}

/// The post-reboot serving contract (the S05 checker's shape): every
/// declaration survived, and a binding that validates implies the tree
/// equals the read-only digest-walk derivation.
fn check_serving_contract(
    node: &Node,
    cells: u16,
    ns: NsId,
    now: Nanos,
    context: &str,
    report: &mut SidecarReport,
) {
    for cell in 0..usize::from(cells) {
        for &(id, path, key_type) in INDEXES {
            let idx = IndexId(id);
            let (servable, generation, degraded) = {
                let ks = node.plane(cell).keyspace();
                let Some(spec) = ks.idx_registry().get_by_id(idx) else {
                    report.violations.push(format!("{context}: cell {cell} lost declaration {id}"));
                    continue;
                };
                (
                    ks.idx_registry().validate_binding(ns, idx, spec.generation).is_ok(),
                    spec.generation,
                    ks.idx_degraded(ns, idx) == Some(true),
                )
            };
            let _ = generation;
            if degraded {
                report.violations.push(format!("{context}: cell {cell} index {id} degraded"));
            }
            if !servable {
                report.refused_bindings += 1;
                continue;
            }
            let program = compile(path.as_bytes()).expect("valid index path");
            let ks = node.plane(cell).keyspace();
            let truth = cell_truth(&ks, ns, &program, key_type, now);
            let tree = cell_tree(&ks, ns, idx);
            report.ready_checks += 1;
            if tree != truth {
                report.violations.push(format!(
                    "{context}: cell {cell} index {id} tree ≠ digest-walk truth \
                     ({} tree entries vs {} derived)",
                    tree.len(),
                    truth.len()
                ));
            }
        }
    }
}

/// Runs one seeded scenario (module docs for the structure).
pub fn run_sidecar_scenario(scenario: &SidecarScenario) -> SidecarReport {
    let clock = Rc::new(VirtualClock::new(Nanos(1)));
    let disk = build_disk(scenario.seed, None);
    let observer = TraceObserver::default();
    let mut rng = SplitMix64::new(scenario.seed ^ 0x51DE_CA55);
    let mut report = SidecarReport::default();
    let mut trace: Vec<u8> = scenario.seed.to_le_bytes().to_vec();
    let base = base(scenario);
    let dir = PathBuf::from("node");

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
                    report.trace_hash = inf_foundation::hash64(&trace, 0x4501_51DE);
                    return report;
                }
            }
        }};
    }

    // ---- setup: DDL + corpus + catalog + live convergence -------------
    let mut node = reboot!("setup");
    if !drive_until!(node, "setup recovery", node.ready()) {
        report.trace_hash = inf_foundation::hash64(&trace, 0x4501_51DE);
        return report;
    }
    let mut client = MiniClient::connect(&mut node, 0);
    let mut call = |node: &mut Node,
                    rng: &mut SplitMix64,
                    report: &mut SidecarReport,
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
            &[b"INF.NS", b"CREATE", b"sc", b"MODE", b"durable", b"FSYNC", b"always"],
            "ns create",
        ) && call(&mut node, &mut rng, &mut report, &[b"INF.NS", b"USE", b"sc"], "ns use");
    if created {
        for i in 0..scenario.docs {
            let key = key_of(i);
            let doc = doc_of(i);
            if !call(&mut node, &mut rng, &mut report, &[b"JSON.SET", &key, b"$", &doc], "corpus") {
                break;
            }
        }
    }
    let Some(ns) = node.plane(0).keyspace().ns_iter().find(|s| s.name == b"sc").map(|s| s.id)
    else {
        report.violations.push("setup: namespace sc missing after DDL".into());
        report.trace_hash = inf_foundation::hash64(&trace, 0x4501_51DE);
        return report;
    };
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
            state: IndexState::Declared,
        });
    }
    node.control.request_persist(catalog);
    for _ in 0..64 {
        if node.step(&mut rng, &clock, &disk, scenario.step_ns_max).is_err() {
            report.violations.push("setup: persist drain failed".into());
        }
        report.scheduler_steps += 1;
    }
    // The declarations reach the live registries only through boot
    // seeding (the DDL fan is S10's) — restart once, then converge.
    disk.power_cut(scenario.seed ^ 0x51DE_0001);
    node = reboot!("post-ddl");
    if !drive_until!(node, "post-ddl recovery", node.ready()) {
        report.trace_hash = inf_foundation::hash64(&trace, 0x4501_51DE);
        return report;
    }
    drive_until!(node, "live convergence", fleet_catalog_ready(&node, scenario.cells));
    trace.extend_from_slice(b"converged");

    // ---- cut 1: mid-checkpoint-write, nothing published ---------------
    let mut op = MiniClient::connect(&mut node, 0);
    let _ = op.call(&mut node, &mut rng, &clock, &disk, scenario.step_ns_max, &[b"INF.CKPT"]);
    drive_until!(node, "stream open", any_ckpt_streaming(&node, scenario.cells));
    // A seeded handful of extra steps advances the stream into its
    // body — across the sweep the cut lands in every phase, sidecar
    // sections included (the file's tail ~40%).
    let extra = rng.next_u64() % 6;
    for _ in 0..extra {
        if node.step(&mut rng, &clock, &disk, scenario.step_ns_max).is_err() {
            break;
        }
        report.scheduler_steps += 1;
    }
    let published_before = manifests_published(&node, scenario.cells);
    disk.power_cut(scenario.seed ^ 0x51DE_0002);
    report.cuts.push(format!("mid-write(extra={extra},published={published_before})"));
    trace.extend_from_slice(&[b'm', extra as u8, published_before as u8]);
    node = reboot!("mid-write");
    if !drive_until!(node, "mid-write recovery", node.ready()) {
        report.trace_hash = inf_foundation::hash64(&trace, 0x4501_51DE);
        return report;
    }
    // No manifest was published on cells cut mid-stream: their boots
    // must take the rebuild path, typed and counted (never a sidecar
    // invented from an unpublished `.ick.new`).
    for cell in 0..usize::from(scenario.cells) {
        let info = node.plane(cell).keyspace().idx_sidecar_info();
        if published_before == 0 && info.loaded != 0 {
            report.violations.push(format!(
                "mid-write: cell {cell} loaded {} sidecars with nothing published",
                info.loaded
            ));
        }
    }
    check_serving_contract(&node, scenario.cells, ns, clock.now(), "mid-write", &mut report);
    drive_until!(node, "rebuild convergence", fleet_catalog_ready(&node, scenario.cells));

    // ---- cut 2: publish under storm, then cut --------------------------
    let mut op = MiniClient::connect(&mut node, 0);
    let _ = op.call(
        &mut node,
        &mut rng,
        &clock,
        &disk,
        scenario.step_ns_max,
        &[b"INF.NS", b"USE", b"sc"],
    );
    let published_before = manifests_published(&node, scenario.cells);
    let _ = op.call(&mut node, &mut rng, &clock, &disk, scenario.step_ns_max, &[b"INF.CKPT"]);
    // Storm while any stream is open: the fuzzy-overlap window the AC
    // names — every one of these mutations is both maintained live in
    // the trees being serialized and tail-replayed under CatchUp.
    for round in 0..2_000u64 {
        if manifests_published(&node, scenario.cells)
            >= published_before + u64::from(scenario.cells)
        {
            break;
        }
        let i = rng.next_u64() % scenario.docs;
        let key = key_of(i);
        let ok = if round % 7 == 6 {
            op.call(&mut node, &mut rng, &clock, &disk, scenario.step_ns_max, &[b"DEL", &key])
        } else {
            let doc = doc_of(i ^ round);
            op.call(
                &mut node,
                &mut rng,
                &clock,
                &disk,
                scenario.step_ns_max,
                &[b"JSON.SET", &key, b"$", &doc],
            )
        };
        match ok {
            Ok(Some(_)) if any_ckpt_streaming(&node, scenario.cells) => {
                report.storm_during_stream += 1;
            }
            Ok(Some(_)) => {}
            _ => break,
        }
    }
    drive_until!(
        node,
        "publish",
        manifests_published(&node, scenario.cells) >= published_before + u64::from(scenario.cells)
    );
    // Tail beyond the checkpoint: mutations after publish, before the cut.
    for round in 0..60u64 {
        let i = rng.next_u64() % scenario.docs;
        let key = key_of(i);
        let doc = doc_of(i ^ (round << 8));
        if op
            .call(
                &mut node,
                &mut rng,
                &clock,
                &disk,
                scenario.step_ns_max,
                &[b"JSON.SET", &key, b"$", &doc],
            )
            .is_err()
        {
            break;
        }
    }
    disk.power_cut(scenario.seed ^ 0x51DE_0003);
    report.cuts.push(format!("post-publish(storm={})", report.storm_during_stream));
    trace.extend_from_slice(&report.storm_during_stream.to_le_bytes());
    node = reboot!("post-publish");
    if !drive_until!(node, "post-publish recovery", node.ready()) {
        report.trace_hash = inf_foundation::hash64(&trace, 0x4501_51DE);
        return report;
    }
    // The load actually happened — per cell, both indexes, zero walk.
    for cell in 0..usize::from(scenario.cells) {
        let ks = node.plane(cell).keyspace();
        let info = ks.idx_sidecar_info();
        report.loaded += u64::from(info.loaded);
        if info.loaded != INDEXES.len() as u32 {
            report.violations.push(format!(
                "post-publish: cell {cell} loaded {}/{} sidecars (rebuilt {})",
                info.loaded,
                INDEXES.len(),
                info.rebuilt
            ));
        }
        let walked = ks.idx_backfill_info().docs_scanned_total;
        if walked != 0 {
            report
                .violations
                .push(format!("post-publish: cell {cell} re-walked {walked} docs after a load"));
        }
    }
    check_serving_contract(&node, scenario.cells, ns, clock.now(), "post-publish", &mut report);
    drive_until!(node, "post-publish ready", fleet_catalog_ready(&node, scenario.cells));
    check_serving_contract(
        &node,
        scenario.cells,
        ns,
        clock.now(),
        "post-publish ready",
        &mut report,
    );

    // ---- the final life: Strict live mutations over loaded trees -------
    let mut op = MiniClient::connect(&mut node, 0);
    let _ = op.call(
        &mut node,
        &mut rng,
        &clock,
        &disk,
        scenario.step_ns_max,
        &[b"INF.NS", b"USE", b"sc"],
    );
    for round in 0..80u64 {
        let i = rng.next_u64() % scenario.docs;
        let key = key_of(i);
        let ok = if round % 6 == 5 {
            op.call(&mut node, &mut rng, &clock, &disk, scenario.step_ns_max, &[b"DEL", &key])
        } else {
            let doc = doc_of(i ^ (round << 16));
            op.call(
                &mut node,
                &mut rng,
                &clock,
                &disk,
                scenario.step_ns_max,
                &[b"JSON.SET", &key, b"$", &doc],
            )
        };
        if ok.is_err() {
            break;
        }
    }
    check_serving_contract(&node, scenario.cells, ns, clock.now(), "final", &mut report);
    if report.storm_during_stream == 0 {
        report.violations.push(
            "no mutation landed while a sidecar-bearing stream was open — the fuzzy-overlap \
             window went unexercised"
                .into(),
        );
    }
    if report.refused_bindings == 0 {
        report.violations.push(
            "no rebuild-window binding refusal was ever observed — the mid-write leg went \
             unexercised"
                .into(),
        );
    }

    for cell in 0..usize::from(scenario.cells) {
        let ks = node.plane(cell).keyspace();
        for &(id, ..) in INDEXES {
            let entries = cell_tree(&ks, ns, IndexId(id)).len() as u64;
            trace.extend_from_slice(&entries.to_le_bytes());
        }
    }
    trace.extend_from_slice(&report.boots.to_le_bytes());
    trace.extend_from_slice(&report.loaded.to_le_bytes());
    trace.extend_from_slice(&report.ready_checks.to_le_bytes());
    trace.extend_from_slice(&(report.violations.len() as u64).to_le_bytes());
    report.trace_hash = inf_foundation::hash64(&trace, 0x4501_51DE);
    report
}
