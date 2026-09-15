//! The durable scenario's audits and lifted regimes: ledger audit, the
//! F-L14-01 lift regimes, survival audit, engagement checks, and the
//! budget oracles.

use super::*;

/// The §8.2 audit of every durable ledger against the recovered node:
/// per key, the required op (the last one acked inside the class's
/// promise before `cut_time`) and the admissible states. With
/// `rebase = Some(boot)` (the transition prelude, ADR-0086 D4 as
/// amended) each audited ledger is then replaced by one synthetic op
/// holding the **recovered** state, acked at `Nanos::ZERO` — recovered
/// state came off the device, so it is required at the next cut
/// regardless of the loss window; the main life's ops append behind it.
/// `Err` = a transport failure already recorded in the report.
#[allow(clippy::too_many_arguments)] // the scheduler tuple the MiniClient calls need
pub(super) fn audit_ledgers(
    node: &mut Node,
    writers: &mut [Writer],
    rng: &mut SplitMix64,
    clock: &Rc<VirtualClock>,
    disk: &SimDisk,
    scenario: &DurableScenario,
    cut_time: Nanos,
    rebase: Option<Nanos>,
    report: &mut DurableReport,
) -> Result<(), ()> {
    let mut audit = MiniClient::connect(node, 0);
    for class in [NsClass::Always, NsClass::Everysec, NsClass::Tiered, NsClass::Indexed] {
        if !writers.iter().any(|w| w.class == class) {
            continue;
        }
        let reply = audit.call(
            node,
            rng,
            clock,
            disk,
            scenario.step_ns_max,
            &[b"INF.NS", b"USE", class.name()],
        );
        if !matches!(reply, Ok(Some(ref ok)) if ok == b"+OK\r\n") {
            report.violations.push(format!("audit USE {class:?} answered {reply:?}"));
            return Err(());
        }
        for writer in writers.iter_mut().filter(|w| w.class == class) {
            for (key, ops) in &mut writer.ledger {
                report.audited_keys += 1;
                let required = required_index(class, ops, cut_time, scenario.clean_stop);
                report.required_ops += required.map_or(0, |i| i as u64 + 1);
                report.allowed_lost_ops += ops.len() as u64 - required.map_or(0, |i| i as u64 + 1);
                let document =
                    scenario.workload == DurableWorkload::Document || class == NsClass::Indexed;
                let command: [&[u8]; 2] = if document { [b"JSON.GET", key] } else { [b"GET", key] };
                let reply = match audit.call(node, rng, clock, disk, scenario.step_ns_max, &command)
                {
                    Ok(Some(reply)) => reply,
                    other => {
                        report.violations.push(format!("audit GET {key:?} answered {other:?}"));
                        return Err(());
                    }
                };
                let admissible: Vec<Vec<u8>> = admissible_states(ops, required)
                    .iter()
                    .map(|state| state.as_ref().map_or(b"$-1\r\n".to_vec(), |v| bulk(v)))
                    .collect();
                if !admissible.contains(&reply) {
                    report.violations.push(format!(
                        "DURABILITY VIOLATION seed {:#x} class {class:?} key {:?}: recovered \
                         {:?} is outside the admissible set (required op index {required:?}, \
                         {} ops, ledger tail: {:?})",
                        scenario.seed,
                        String::from_utf8_lossy(key),
                        String::from_utf8_lossy(&reply),
                        ops.len(),
                        ops.iter()
                            .rev()
                            .take(3)
                            .map(|op| (
                                op.state_after
                                    .as_ref()
                                    .map(|v| String::from_utf8_lossy(v).into_owned()),
                                op.acked_at
                            ))
                            .collect::<Vec<_>>()
                    ));
                }
                if let Some(boot) = rebase {
                    debug_assert_eq!(scenario.workload, DurableWorkload::KeyValue);
                    let recovered = parse_bulk(&reply).unwrap_or_else(|| {
                        panic!("audit GET {key:?} answered a non-bulk reply {reply:?}")
                    });
                    *ops = vec![OpRec {
                        state_after: recovered,
                        sent_at: boot,
                        acked_at: Some(Nanos::ZERO),
                    }];
                }
            }
        }
    }
    Ok(())
}

/// The lift regime's index (F-L14-01): `$.meta.tag`, the document
/// model's always-present integer member.
const LIFT_INDEX: (u32, &str, IndexKeyType) = (1, "$.meta.tag", IndexKeyType::I64);

/// The lift regime's DDL: a tiered `always` namespace (the `m4-tiered`
/// budget shape — demotion, extents and displacement pairs inside a
/// short run) and an indexed `always` document namespace whose index is
/// declared through the production catalog swap and converged before
/// any traffic.
#[allow(clippy::too_many_arguments)] // the scheduler tuple the MiniClient calls need
pub(super) fn lift_regime_ddl(
    node: &mut Node,
    setup: &mut MiniClient,
    rng: &mut SplitMix64,
    clock: &Rc<VirtualClock>,
    disk: &SimDisk,
    scenario: &DurableScenario,
    report: &mut DurableReport,
) -> Result<LiftNs, String> {
    let tier: &[&[u8]] = &[
        b"INF.NS",
        b"CREATE",
        NsClass::Tiered.name(),
        b"MODE",
        b"durable",
        b"FSYNC",
        b"always",
        b"MEM-BUDGET",
        b"3mb",
        b"MUTABLE-FRACTION",
        b"100",
        b"MAINTAIN-SLICE",
        b"1mb",
        b"BLOB-THRESHOLD",
        b"4kb",
        b"TIER-IO-MODE",
        b"buffered",
    ];
    let idx: &[&[u8]] =
        &[b"INF.NS", b"CREATE", NsClass::Indexed.name(), b"MODE", b"durable", b"FSYNC", b"always"];
    for create in [tier, idx] {
        match setup.call(node, rng, clock, disk, scenario.step_ns_max, create) {
            Ok(Some(ok)) if ok == b"+OK\r\n" => {}
            other => return Err(format!("lift-regime DDL {:?} answered {other:?}", create[2])),
        }
    }
    let ns_named =
        |name: &[u8]| node.plane(0).keyspace().ns_iter().find(|s| s.name == name).map(|s| s.id);
    let ns = ns_named(NsClass::Indexed.name())
        .ok_or_else(|| "lift-regime: idx namespace missing after DDL".to_owned())?;
    let tier_ns = ns_named(NsClass::Tiered.name())
        .ok_or_else(|| "lift-regime: tier namespace missing after DDL".to_owned())?;
    let (id, path, key_type) = LIFT_INDEX;
    let program = compile(path.as_bytes()).expect("valid index path").as_bytes().to_vec();
    let mut catalog = node.plane(0).keyspace().export_catalog(node.control.next_ns_id(), 2, 2);
    catalog.index.entries.push(IndexSpec {
        id: IndexId(id),
        generation: u64::from(id),
        ns,
        name: b"by-tag".to_vec(),
        program,
        key_type,
        state: IndexState::Declared,
    });
    node.control.request_persist(catalog);
    // The swap lands on disk here; the live registries see the
    // declaration at the transition boot (boot-seeding — the live DDL fan
    // is S10's), and the S05 machine converges during the main life.
    for _ in 0..64 {
        node.step(rng, clock, disk, scenario.step_ns_max)
            .map_err(|e| format!("lift-regime persist: {e}"))?;
        report.scheduler_steps += 1;
    }
    Ok(LiftNs { tier: tier_ns, idx: ns })
}

/// The plant's first precondition (batch 21): a clean restart in the
/// prelude's class so the persisted index declaration boot-seeds into
/// every cell's registry (no cut — the page cache is the OS's; a process
/// restart, not a crash). The log quiesces first so nothing staged rides
/// the drop.
pub(super) fn lift_regime_seed_life(
    node: Node,
    first_life: &DurableScenario,
    disk: &SimDisk,
    clock: &Rc<VirtualClock>,
    observer: &TraceObserver,
) -> Result<Node, String> {
    let mut node = node;
    let mut rng = SplitMix64::new(first_life.seed ^ 0x5EED_11F7);
    let mut quiet_steps = 0u64;
    for _ in 0..STALL_STEPS {
        node.step(&mut rng, clock, disk, first_life.step_ns_max)
            .map_err(|e| format!("lift seed life: quiesce: {e}"))?;
        let quiet = (0..usize::from(first_life.cells)).all(|cell| {
            node.plane(cell)
                .durable_stats()
                .is_none_or(|s| s.frames_in_flight_now == 0 && s.records_staged == 0)
        });
        quiet_steps = if quiet { quiet_steps + 1 } else { 0 };
        if quiet_steps >= 64 {
            break;
        }
    }
    if quiet_steps < 64 {
        return Err("lift seed life: the log never quiesced before the restart".to_owned());
    }
    drop(node);
    let mut node = boot(first_life, PathBuf::from("node"), disk, clock, observer)
        .map_err(|e| format!("lift seed boot refused: {e}"))?;
    for _ in 0..STALL_STEPS {
        if node.ready() {
            return Ok(node);
        }
        node.step(&mut rng, clock, disk, first_life.step_ns_max)
            .map_err(|e| format!("lift seed boot failed: {e}"))?;
    }
    Err("lift seed boot: recovery stalled".to_owned())
}

/// The plant's second precondition: the boot-seeded index converges,
/// every cell owns a few indexed seed documents, and a checkpoint
/// requested on every cell publishes — so the transition boot loads a
/// sidecar with entries, the state the commit-ordering defect needs.
#[allow(clippy::too_many_arguments)] // the scheduler tuple the MiniClient calls need
pub(super) fn lift_regime_seed_checkpoint(
    node: &mut Node,
    rng: &mut SplitMix64,
    clock: &Rc<VirtualClock>,
    disk: &SimDisk,
    scenario: &DurableScenario,
    report: &mut DurableReport,
) -> Result<(), String> {
    let (id, _, _) = LIFT_INDEX;
    let id = IndexId(id);
    let mut ready = false;
    for _ in 0..STALL_STEPS {
        ready = (0..usize::from(scenario.cells)).all(|cell| {
            node.plane(cell).keyspace().idx_registry().cell_state(id) == Some(IndexState::Ready)
        });
        if ready {
            break;
        }
        node.step(rng, clock, disk, scenario.step_ns_max)
            .map_err(|e| format!("lift seed: index convergence: {e}"))?;
        report.scheduler_steps += 1;
    }
    if !ready {
        return Err("lift seed: the boot-seeded index never converged".to_owned());
    }
    for cell in 0..usize::from(scenario.cells) {
        let mut client = MiniClient::connect(node, cell);
        let reply = client.call(
            node,
            rng,
            clock,
            disk,
            scenario.step_ns_max,
            &[b"INF.NS", b"USE", NsClass::Indexed.name()],
        );
        if !matches!(reply, Ok(Some(ref ok)) if ok == b"+OK\r\n") {
            return Err(format!("lift seed: USE idx on cell {cell} answered {reply:?}"));
        }
        for n in 0..3usize {
            let key = crate::lift::local_key(&format!("lift:seed{n}"), cell, scenario.cells);
            let text = crate::lift::planted_doc_text(i64::try_from(cell * 8 + n).expect("small"));
            let reply = client.call(
                node,
                rng,
                clock,
                disk,
                scenario.step_ns_max,
                &[b"JSON.SET", &key, b"$", &text],
            );
            if !matches!(reply, Ok(Some(ref ok)) if ok == b"+OK\r\n") {
                return Err(format!("lift seed: JSON.SET on cell {cell} answered {reply:?}"));
            }
        }
    }
    let epoch = node.control.request_ckpt_all();
    for _ in 0..STALL_STEPS {
        if node.control.ckpt_board().min_published() >= epoch {
            return Ok(());
        }
        node.step(rng, clock, disk, scenario.step_ns_max)
            .map_err(|e| format!("lift seed: checkpoint: {e}"))?;
        report.scheduler_steps += 1;
    }
    Err(format!("lift seed: checkpoint epoch {epoch} never published on every cell"))
}

/// The plant's oracle after the transition boot (batch 21): every
/// planted cell lifted exactly the planted residue (the board's
/// `stale_residue_slacks`), loaded its sidecar, its tree equals the
/// scan-derived truth with the planted document in it, the lifted tiered
/// record and document serve, and the discarded life's residue never
/// does. A cell that did not lift or load is a vacuous plant — red.
#[allow(clippy::too_many_arguments)] // the scheduler tuple the MiniClient calls need
pub(super) fn lift_plant_oracle(
    node: &mut Node,
    planted: &[crate::lift::PlantedCell],
    rng: &mut SplitMix64,
    clock: &Rc<VirtualClock>,
    disk: &SimDisk,
    scenario: &DurableScenario,
    report: &mut DurableReport,
) {
    let seed = scenario.seed;
    for plant in planted {
        let cell = u16::try_from(plant.cell).expect("cell fits u16");
        let lifted = node.control.recovery_board().slot(cell).residue().stale_residue_slacks;
        let loaded = u64::from(node.plane(plant.cell).keyspace().idx_sidecar_info().loaded);
        report.lift_plant_lifts += lifted;
        report.lift_plant_sidecars += loaded;
        if lifted == 0 {
            report.violations.push(format!(
                "LIFT PLANT VACUOUS seed {seed:#x} cell {}: the transition boot lifted no residue \
                 (planted segment {} beyond segment {})",
                plant.cell, plant.lifted_segment.0, plant.residue_segment.0
            ));
        }
        if loaded == 0 {
            report.violations.push(format!(
                "LIFT PLANT VACUOUS seed {seed:#x} cell {}: no index sidecar loaded at the \
                 transition boot (the forced checkpoint carried none)",
                plant.cell
            ));
        }
    }
    lift_regime_index_oracle(node, rng, clock, disk, scenario, "after the transition boot", report);
    let (_, path, key_type) = LIFT_INDEX;
    let program = compile(path.as_bytes()).expect("valid index path");
    let now = clock.now();
    for plant in planted {
        let ks = node.plane(plant.cell).keyspace();
        let Some(ns) = ks.ns_iter().find(|s| s.name == NsClass::Indexed.name()).map(|s| s.id)
        else {
            continue;
        };
        let truth = crate::backfill::cell_truth(&ks, ns, &program, key_type, now);
        let hash = ks.hasher().hash(&plant.doc_key);
        if !truth.iter().any(|(_, h)| *h == hash) {
            report.violations.push(format!(
                "LIFT PLANT VIOLATION seed {seed:#x} cell {}: the lifted document {:?} is not in \
                 the recovered store",
                plant.cell,
                String::from_utf8_lossy(&plant.doc_key)
            ));
        }
        drop(ks);
        let mut client = MiniClient::connect(node, plant.cell);
        let expect: [(&[u8], &[u8], Vec<u8>); 3] = [
            (NsClass::Tiered.name(), &plant.tier_key, bulk(crate::lift::LIFTED_VALUE)),
            (NsClass::Tiered.name(), &plant.ghost_key, b"$-1\r\n".to_vec()),
            (
                NsClass::Indexed.name(),
                &plant.doc_key,
                bulk(&crate::lift::planted_doc_text(plant.tag)),
            ),
        ];
        for (ns_name, key, want) in expect {
            let reply = client.call(
                node,
                rng,
                clock,
                disk,
                scenario.step_ns_max,
                &[b"INF.NS", b"USE", ns_name],
            );
            if !matches!(reply, Ok(Some(ref ok)) if ok == b"+OK\r\n") {
                report.violations.push(format!(
                    "lift plant: USE {} answered {reply:?}",
                    String::from_utf8_lossy(ns_name)
                ));
                return;
            }
            let read: &[u8] = if ns_name == NsClass::Indexed.name() { b"JSON.GET" } else { b"GET" };
            let reply = client.call(node, rng, clock, disk, scenario.step_ns_max, &[read, key]);
            if !matches!(reply, Ok(Some(ref got)) if *got == want) {
                report.violations.push(format!(
                    "LIFT PLANT VIOLATION seed {seed:#x} cell {}: {} {:?} answered {:?}, want {:?}",
                    plant.cell,
                    String::from_utf8_lossy(read),
                    String::from_utf8_lossy(key),
                    reply
                        .as_ref()
                        .ok()
                        .and_then(|r| r.as_ref())
                        .map(|r| String::from_utf8_lossy(r).into_owned()),
                    String::from_utf8_lossy(&want)
                ));
            }
        }
    }
}

/// The lift regime's index oracle after a lifting boot: once every
/// cell's machine reads Ready again (a loaded sidecar caught up on the
/// tail, or the S05 rebuild ran), each cell's tree equals the
/// scan-derived truth over its recovered documents and no index is
/// degraded — the lifted documents' entries included. Pre-batch-19 the
/// sidecar committed before the lifted segments replayed, so a loaded
/// tree missed exactly those documents.
#[allow(clippy::too_many_arguments)] // the scheduler tuple the MiniClient calls need
pub(super) fn lift_regime_index_oracle(
    node: &mut Node,
    rng: &mut SplitMix64,
    clock: &Rc<VirtualClock>,
    disk: &SimDisk,
    scenario: &DurableScenario,
    context: &str,
    report: &mut DurableReport,
) {
    let Some(ns) = node
        .plane(0)
        .keyspace()
        .ns_iter()
        .find(|s| s.name == NsClass::Indexed.name())
        .map(|s| s.id)
    else {
        report.violations.push(format!(
            "LIFT REGIME VIOLATION seed {:#x}: the idx namespace did not survive the cut",
            scenario.seed
        ));
        return;
    };
    let (id, path, key_type) = LIFT_INDEX;
    let id = IndexId(id);
    let mut ready = false;
    for _ in 0..STALL_STEPS {
        ready = (0..usize::from(scenario.cells)).all(|cell| {
            node.plane(cell).keyspace().idx_registry().cell_state(id) == Some(IndexState::Ready)
        });
        if ready {
            break;
        }
        if node.step(rng, clock, disk, scenario.step_ns_max).is_err() {
            break;
        }
        report.scheduler_steps += 1;
    }
    if !ready {
        report.violations.push(format!(
            "LIFT REGIME VIOLATION seed {:#x}: the index never re-converged {context}",
            scenario.seed
        ));
        return;
    }
    let program = compile(path.as_bytes()).expect("valid index path");
    let now = clock.now();
    for cell in 0..usize::from(scenario.cells) {
        let ks = node.plane(cell).keyspace();
        let truth = crate::backfill::cell_truth(&ks, ns, &program, key_type, now);
        let tree = crate::backfill::cell_tree(&ks, ns, id);
        if tree != truth {
            let cell16 = u16::try_from(cell).expect("cell fits u16");
            report.violations.push(format!(
                "LIFT REGIME INDEX VIOLATION seed {:#x} cell {cell}: index tree ≠ scan-derived \
                 truth {context} ({} tree entries vs {} derived; sidecars loaded {}, stale \
                 slacks lifted {})",
                scenario.seed,
                tree.len(),
                truth.len(),
                ks.idx_sidecar_info().loaded,
                node.control.recovery_board().slot(cell16).residue().stale_residue_slacks
            ));
        }
        if ks.idx_degraded(ns, id) == Some(true) {
            report.violations.push(format!(
                "LIFT REGIME INDEX VIOLATION seed {:#x} cell {cell}: index degraded {context}",
                scenario.seed
            ));
        }
    }
}

/// A RESP bulk reply → the value (`None` for the null bulk); `None` for
/// anything that is not a bulk string.
fn parse_bulk(reply: &[u8]) -> Option<Option<Vec<u8>>> {
    if reply == b"$-1\r\n" {
        return Some(None);
    }
    let rest = reply.strip_prefix(b"$")?;
    let nl = rest.iter().position(|&b| b == b'\r')?;
    let len: usize = std::str::from_utf8(&rest[..nl]).ok()?.parse().ok()?;
    let body = rest.get(nl + 2..nl + 2 + len)?;
    Some(Some(body.to_vec()))
}

/// The §8.2 survival audit for a legally-refused boot (ADR-0021 D3):
/// reconstructs each cell's recoverable prefix — manifest → named `.ick`
/// → tail replay from begin, stopping at the first invalid frame — and
/// audits every durable ledger key against the admissible-state rule.
/// Sound because fsync-covered bytes always survive the sim disk's cut:
/// on an honest node the corruption point lies strictly above the
/// watermark, so the prefix contains everything the promise binds; a
/// lying fsync scatters required data past the gap and is caught here.
pub(crate) fn survival_audit(
    scenario: &DurableScenario,
    disk: &SimDisk,
    writers: &[Writer],
    cut_time: Nanos,
    tally: &mut AuditTally,
) {
    debug_assert_eq!(scenario.workload, DurableWorkload::KeyValue);
    let data_dir = PathBuf::from("node");
    let catalog = match load_catalog_from(disk, &data_dir) {
        Ok(Some(catalog)) => catalog,
        other => {
            tally.violations.push(format!(
                "SURVIVAL VIOLATION seed {:#x}: acked DDL lost — catalog unreadable after the \
                 cut ({other:?})",
                scenario.seed
            ));
            return;
        }
    };
    let mut ks =
        Keyspace::new(StoreConfig { hasher: node_hasher(scenario.seed), ..Default::default() });
    if let Err(err) = ks.seed_catalog(&catalog) {
        tally.violations.push(format!("survival audit: seed_catalog failed: {err:?}"));
        return;
    }
    let now = cut_time;
    let anchor = WallAnchor { internal_ms: 0, unix_ms: 0 };

    for cell in 0..scenario.cells {
        let shard = data_dir.join(format!("shard-{cell}"));
        let log_dir = shard.join("log");
        let manifest = match read_manifest(disk, &shard) {
            Ok(manifest) => manifest,
            Err(err) => {
                tally.violations.push(format!(
                    "SURVIVAL VIOLATION seed {:#x} cell {cell}: MANIFEST unreadable after the \
                     cut: {err}",
                    scenario.seed
                ));
                continue;
            }
        };
        if let Some(manifest) = &manifest {
            let ick = shard.join("ckpt").join(ick_file_name(manifest.ckpt_id));
            let loaded = read_ick(disk, &ick, IckReaderConfig::default(), |record| {
                ks.apply_record(&record, now, anchor).map(|_| ()).map_err(|e| format!("{e:?}"))
            });
            if let Err(err) = loaded {
                tally.violations.push(format!(
                    "SURVIVAL VIOLATION seed {:#x} cell {cell}: the manifest-named checkpoint \
                     is unreadable (fsync-covered loss): {err:?}",
                    scenario.seed
                ));
                continue;
            }
        }
        let begin = manifest.as_ref().map(|m| m.begin_lsn);
        let floor = manifest.as_ref().map_or(SegmentId(0), inf_log::Manifest::floor);
        let scan = match scan_log_dir_from(disk, &log_dir, floor) {
            Ok(outcome) => outcome.scan,
            Err(err) => {
                tally
                    .violations
                    .push(format!("survival audit: cell {cell} log scan failed: {err:?}"));
                continue;
            }
        };
        // Strict prefix: stop at the first invalid frame anywhere — on an
        // honest node everything required is below it (watermark ≤ gap).
        'segments: for &segment in scan.segments() {
            let Ok(mut reader) =
                SegmentReader::open(disk, &log_dir, segment, ReaderConfig::default())
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
                            let _ = ks.apply_record(&record, now, anchor);
                        }
                    }
                    Ok(None) => break,
                    Err(_) => break 'segments,
                }
            }
        }
    }

    // Audit the durable ledgers directly against the reconstructed state.
    let ns_of = |name: &[u8]| -> Option<NsId> {
        catalog.entries.iter().find(|spec| spec.name == name).map(|spec| spec.id)
    };
    for class in [NsClass::Always, NsClass::Everysec] {
        let Some(ns) = ns_of(class.name()) else {
            tally.violations.push(format!(
                "SURVIVAL VIOLATION seed {:#x}: acked CREATE for {class:?} lost from the catalog",
                scenario.seed
            ));
            continue;
        };
        let Some(store) = ks.ns_store_mut(ns) else {
            tally.violations.push(format!("survival audit: ns {ns:?} has no store"));
            continue;
        };
        for writer in writers.iter().filter(|w| w.class == class) {
            for (key, ops) in &writer.ledger {
                let required = required_index(class, ops, cut_time, scenario.clean_stop);
                tally.count(ops, required);
                let got = store.get(key, now).map(<[u8]>::to_vec);
                let admissible = admissible_states(ops, required);
                if !admissible.contains(&got) {
                    tally.violations.push(format!(
                        "SURVIVAL VIOLATION seed {:#x} class {class:?} key {:?}: surviving \
                         image holds {:?}, outside the admissible set (required op index \
                         {required:?}, {} ops)",
                        scenario.seed,
                        String::from_utf8_lossy(key),
                        got.as_ref().map(|v| String::from_utf8_lossy(v).into_owned()),
                        ops.len()
                    ));
                }
            }
        }
    }
}

/// Batch 34 — an armed knob that never fired proves nothing: every
/// seed-cadence arm of the durable shapes asserts its own engagement at
/// the cut (the counters are life-scoped, so this runs before the node
/// drops). Each rule names the activity a life must have reached for the
/// arm to be reachable at all — below it the arm is disclosed, not judged.
pub(super) fn engagement_checks(
    scenario: &DurableScenario,
    node: &Node,
    report: &mut DurableReport,
) {
    let seed = scenario.seed;
    for cell in 0..usize::from(scenario.cells) {
        let Some(stats) = node.plane(cell).durable_stats() else { continue };
        report.ckpt_bound_splits += stats.ckpt_bound_splits;
        // ADR-0117 D4: under the m2 bound one image fills a section, so
        // a life that completed a checkpoint after a working set of
        // records must have sealed for the bound (two live keys in one
        // namespace suffice).
        let (completed, _) = node.plane(cell).ckpt_stats_for_sim();
        if let Some(bound) = scenario.ckpt_section_bound
            && completed > 0
            && stats.records_appended >= 64
            && stats.ckpt_bound_splits == 0
        {
            report.violations.push(format!(
                "SECTION-BOUND ARM VACUOUS seed {seed:#x} cell {cell}: {completed} checkpoints \
                 after {} records under a {bound} B bound, no section sealed for it",
                stats.records_appended
            ));
        }
        let active = stats.log_frame_bytes >= 4 * u64::from(scenario.segment_bytes);
        // M4.5-S39a: the fill policy holds aligned frames — an active
        // `Direct` life must have held at least once (the reorder shape
        // asserts the same below with its own wording).
        if scenario.fill.enabled()
            && scenario.io_mode == SegmentIoMode::Direct
            && active
            && stats.frame_waits_fill == 0
        {
            report.violations.push(format!(
                "FILL ARM VACUOUS seed {seed:#x} cell {cell}: {} frame bytes on the aligned \
                     class, the policy never held a frame",
                stats.log_frame_bytes
            ));
        }
        // M4.5-S43: the group hold rides packed segments — an active
        // `Buffered` life must have held at least once.
        if scenario.group.enabled()
            && scenario.io_mode == SegmentIoMode::Buffered
            && active
            && stats.frame_waits_group == 0
        {
            report.violations.push(format!(
                "GROUP-HOLD ARM VACUOUS seed {seed:#x} cell {cell}: {} frame bytes on the \
                     packed class, the hold never held a frame",
                stats.log_frame_bytes
            ));
        }
    }
    // A positive control that never fired proves nothing about the
    // oracle it exists to trip.
    report.plant_fired |= node.nets.iter().any(|net| net.borrow().plant_fired());
    if scenario.plant != Plant::None && !report.plant_fired {
        report.violations.push(format!(
            "PLANT VACUOUS seed {seed:#x}: {:?} was requested and never fired",
            scenario.plant
        ));
    }
}

/// The device-budget oracles (M4.5-S36, ADR-0088 D8), evaluated per cell
/// at the cut on the budget's own ledger and the sim driver's observed
/// bytes. Every failure is a named violation; every disclosure rides the
/// report.
pub(super) fn budget_oracles(
    scenario: &DurableScenario,
    node: &Node,
    now: Nanos,
    report: &mut DurableReport,
) {
    use inf_runtime::{BURST_HORIZON_NS, IoClass};
    let stall = scenario.stall.as_ref().expect("the budget scenario arms a disk model");
    let share = scenario.device.model_share;
    let elapsed_s = now.0.saturating_sub(1) as f64 / 1e9;
    for cell in 0..usize::from(scenario.cells) {
        let Some(stats) = node.plane(cell).durable_stats() else {
            report.violations.push(format!("cell {cell}: no durable plane in the budget scenario"));
            continue;
        };
        let observed = node.cells[cell].0.driver().observed_io();
        let ckpt_slice = f64::from(scenario.ckpt_section_bytes.unwrap_or(256 << 10) + 4096);
        // (a) Accounting identity: what the budget counted is what the
        // driver saw, for every token-classed class — up to the ops the
        // cut caught between push and submit (`LoopCx::push` queues; the
        // driver drains at the *next* iteration's `submit_and_reap`, and
        // the cut eats that queue by design): at most one op per class
        // and one op's bytes (a segment for frames, a block for
        // checkpoint and zero-fill), never fewer than the driver saw.
        // The two cold-read classes together match the driver's reads.
        let one_op_bytes = |class: IoClass| -> u64 {
            match class {
                IoClass::LogFrame => u64::from(scenario.segment_bytes),
                IoClass::ZeroFill => 256 << 10,
                IoClass::Checkpoint => ckpt_slice as u64,
                _ => 0,
            }
        };
        for class in IoClass::ALL {
            let counted = stats.io_budget[class.index()];
            match class {
                IoClass::BlobWrite | IoClass::ColdReadForeground | IoClass::ColdReadMaintain => {}
                _ => {
                    let seen = observed.bytes[class.index()];
                    let seen_ops = observed.ops[class.index()];
                    let ops_slack = counted.spent_ops.wrapping_sub(seen_ops);
                    let bytes_slack = counted.spent_bytes.wrapping_sub(seen);
                    if counted.spent_bytes < seen || bytes_slack > one_op_bytes(class) {
                        report.violations.push(format!(
                            "cell {cell}: io_budget_bytes_{} = {} but the driver saw {seen} \
                             (ADR-0088 D8 accounting identity; slack ≤ one op's bytes)",
                            class.name(),
                            counted.spent_bytes
                        ));
                    }
                    if counted.spent_ops < seen_ops || ops_slack > 3 {
                        report.violations.push(format!(
                            "cell {cell}: io_budget_ops_{} = {} but the driver saw {seen_ops} \
                             (slack ≤ one LOG step's pushed-unsubmitted ops)",
                            class.name(),
                            counted.spent_ops
                        ));
                    }
                }
            }
        }
        let reads_counted = stats.io_budget[IoClass::ColdReadForeground.index()].spent_bytes
            + stats.io_budget[IoClass::ColdReadMaintain.index()].spent_bytes;
        if reads_counted < observed.read_bytes || reads_counted - observed.read_bytes > 16 << 10 {
            report.violations.push(format!(
                "cell {cell}: cold-read bytes counted {reads_counted} but the driver saw {}",
                observed.read_bytes
            ));
        }
        // (b) Rate bound — the budget's contract: background bytes over
        // the run never exceed the share's grant plus two burst horizons
        // (the class caps plus the pool) plus one slice per class (the
        // cap floor) plus the checkpoint keep-up floor (the log's bytes
        // over α — ADR-0088 D2 amended). Foreground subtraction only
        // lowers the weighted grant.
        let horizon_bytes = share.write_bytes_per_s as f64 * BURST_HORIZON_NS as f64 / 1e9;
        let slices = 256.0 * 1024.0 + ckpt_slice + 1024.0 * 1024.0 + 16.0 * 1024.0;
        let keepup = stats.log_frame_bytes as f64 / 2.0;
        let bound =
            share.write_bytes_per_s as f64 * elapsed_s + 2.0 * horizon_bytes + slices + keepup;
        let background: u64 = IoClass::ALL
            .iter()
            .filter(|c| !c.is_foreground() && !c.is_read())
            .map(|c| stats.io_budget[c.index()].spent_bytes)
            .sum();
        if background as f64 > bound {
            report.violations.push(format!(
                "cell {cell}: background wrote {background} bytes in {elapsed_s:.3} s against a \
                 share of {} B/s — bound {bound:.0} (ADR-0088 D2 rate bound)",
                share.write_bytes_per_s
            ));
        }
        // (c) Engagement: the regime is not vacuous — some background
        // class was deferred at least once (the S27 lesson: a pressure
        // row whose counter stayed at 0 measured nothing). The checkpoint
        // class itself is floored to keep up with the log (ADR-0088 D2
        // amended), so its deferrals are not the signal; zero-fill's are.
        let background_deferrals: u64 = IoClass::ALL
            .iter()
            .filter(|c| !c.is_foreground())
            .map(|c| stats.io_budget[c.index()].deferrals)
            .sum();
        if background_deferrals == 0 {
            let ckpt = node.plane(cell).ckpt_stats_for_sim();
            report.violations.push(format!(
                "cell {cell}: the budget never deferred a background block — the scenario's \
                 offered load did not exceed the share (vacuous regime); ckpts completed {} \
                 aborted {} in_progress {} interval {} records_since_begin {} log_frame_bytes {} \
                 ckpt_bytes {} zero_fill {} rotations_unzeroed {} waits_pace {} \
                 deferrals[zero_fill {} tier_flush {} ckpt {}] frames_in_flight_max {}",
                ckpt.0,
                ckpt.1,
                stats.ckpt_in_progress,
                stats.ckpt_interval_bytes,
                stats.ckpt_records_since_begin,
                stats.log_frame_bytes,
                stats.ckpt_bytes_total,
                stats.zero_fill_bytes,
                stats.rotations_unzeroed,
                stats.frame_waits_pace,
                stats.io_budget[IoClass::ZeroFill.index()].deferrals,
                stats.io_budget[IoClass::TierFlush.index()].deferrals,
                stats.io_budget[IoClass::Checkpoint.index()].deferrals,
                stats.frames_in_flight_max,
            ));
        }
        // (d) Progress: deferrals never starved the classes — a
        // checkpoint published and the zero-fill landed its bytes.
        if stats.io_budget[IoClass::Checkpoint.index()].spent_bytes == 0 {
            report.violations.push(format!("cell {cell}: no checkpoint bytes reached the device"));
        }
        if stats.zero_fill_bytes == 0 {
            report.violations.push(format!("cell {cell}: no zero-fill bytes reached the device"));
        }
        // (e) Foreground bound (physics, both modes): a frame waits for at
        // most its own transfer, one background block ahead on the byte
        // timeline, the budget's burst, the write-through base and tail,
        // and one scheduler step of completion quantization.
        let rate = stall.write_bytes_per_s as f64;
        let frame_max = f64::from(scenario.segment_bytes);
        let block_max = (256.0_f64 * 1024.0).max(ckpt_slice);
        let service_us = (stall.through_base_ns * (1 + stall.tail_mult)) as f64 / 1e3;
        let bound_us = (frame_max + block_max + horizon_bytes) / rate * 1e6
            + service_us
            + scenario.step_ns_max as f64 / 1e3
            + 1_000.0;
        if stats.write_stall_max_us as f64 > bound_us {
            report.violations.push(format!(
                "cell {cell}: worst frame write latency {} µs exceeds the foreground bound \
                 {bound_us:.0} µs (ADR-0088 D8)",
                stats.write_stall_max_us
            ));
        }
        // Disclosures: the model must be present and the pipeline must
        // have filled, or the scenario measured a different machine.
        if stats.io_budget_model_absent == 1 {
            report.violations.push(format!("cell {cell}: the budget model is absent"));
        }
    }
}

pub(super) fn finish(
    mut report: DurableReport,
    observer: &TraceObserver,
    clock: &Rc<VirtualClock>,
) -> DurableReport {
    report.trace = observer.0.borrow().clone();
    report.trace_hash = hash64(&report.trace, 0xD07A);
    report.sim_seconds = clock.now().0.saturating_sub(1) as f64 / 1e9;
    report
}
