//! The durable scenario's phase script (`run_durable_scenario`): seeded
//! traffic, cuts, reboots, and the per-phase oracles it drives.

use super::*;

// ---- the run ---------------------------------------------------------------

/// Runs one seeded durable scenario: boot → DDL → seeded traffic → power
/// cut mid-run → reboot (optionally cut again mid-recovery) → recover →
/// audit every ledger key against the §8.2 admissible-state rule.
#[allow(clippy::too_many_lines, reason = "shape: one linear phase script, like run_scenario")]
#[must_use]
fn run_observed(scenario: &DurableScenario, observer: TraceObserver) -> DurableReport {
    let clock = Rc::new(VirtualClock::new(Nanos(1)));
    let disk = build_disk(scenario.seed, scenario.stall.as_ref());
    if let Some(allowed) = scenario.ckpt_direct_refused_after {
        disk.refuse_direct_writes_after(allowed);
    }
    let mut rng = SplitMix64::new(scenario.seed ^ 0xD07A_B1E5);
    let mut report = DurableReport {
        trace: Vec::new(),
        trace_hash: 0,
        state_hash: 0,
        violations: Vec::new(),
        cap_rows_checked: 0,
        cap_rows_crossed: 0,
        stalled: false,
        clean_stop_steps: 0,
        clean_stop_replay_records: 0,
        commands_done: 0,
        sim_seconds: 0.0,
        required_ops: 0,
        allowed_lost_ops: 0,
        audited_keys: 0,
        scheduler_steps: 0,
        refused_boot: false,
        always_ack_latency_ms_max: 0,
        equivalence_checks: 0,
        documents_compared: 0,
        corpus_documents_used: 0,
        cut_classes: Vec::new(),
        frames_in_flight_max: 0,
        frame_waits_barrier: 0,
        frame_waits_rotation: 0,
        frame_waits_reorder: 0,
        frame_waits_ledger: 0,
        frame_waits_ack_map: 0,
        ledger_prealloc_deferrals: 0,
        ckpt_downgrades: 0,
        ckpt_bound_splits: 0,
        plant_fired: false,
        frame_waits_fill: 0,
        frame_waits_group: 0,
        idle_tick_violations: 0,
        hold_episode_violations: 0,
        frames_awaiting_max: 0,
        fsync_entries_max: 0,
        write_through_entries_max: 0,
        budget_background_bytes: 0,
        budget_deferrals: 0,
        budget_unattainable: 0,
        ckpt_block_wait_ns_max: 0,
        frame_waits_pace: 0,
        write_stall_max_us: 0,
        reopened_packed_tails: 0,
        segments_recycled: 0,
        recycle_misses: 0,
        recycle_fallbacks: 0,
        recycle_served: 0,
        recycle_open_unreached: false,
        recycle_sentinels: 0,
        segment_rotations: 0,
        recycled_residue_slacks: 0,
        clean_stop_torn_tails: 0,
        recycle_waits_started: 0,
        recycle_waits_satisfied: 0,
        recycle_waits_expired: 0,
        segment_inline_preallocs: 0,
        lift_regime: scenario.lift_regime,
        lift_tiered_ops: 0,
        lift_indexed_ops: 0,
        lift_sidecars_loaded: 0,
        stale_residue_slacks: 0,
        lift_plants: 0,
        torn_plants: 0,
        lift_plant_lifts: 0,
        lift_plant_sidecars: 0,
        tier_budget_cells: Vec::new(),
        epoch_marks_cells: Vec::new(),
        ledger_wedge: None,
    };
    let fail = |report: &mut DurableReport, what: String| {
        report.violations.push(what);
    };

    // ---- boot 1 + DDL ------------------------------------------------
    // With a prelude the first life runs in the prelude's barrier class
    // (ADR-0086 D4 as amended); the scenario's own class boots at the
    // clean restart below.
    let first_life = match scenario.prelude {
        Some(prelude) => {
            let mut first = scenario.clone();
            first.io_mode = prelude.io_mode;
            first
        }
        None => scenario.clone(),
    };
    let mut node = match boot(&first_life, PathBuf::from("node"), &disk, &clock, &observer) {
        Ok(node) => node,
        Err(err) => {
            fail(&mut report, format!("boot 1 failed: {err}"));
            return finish(report, &observer, &clock);
        }
    };
    let mut setup = MiniClient::connect(&mut node, 0);
    for (name, class) in [(b"alw".as_slice(), b"always".as_slice()), (b"esec", b"everysec")] {
        let reply = setup.call(
            &mut node,
            &mut rng,
            &clock,
            &disk,
            scenario.step_ns_max,
            &[b"INF.NS", b"CREATE", name, b"MODE", b"durable", b"FSYNC", class],
        );
        match reply {
            Ok(Some(ok)) if ok == b"+OK\r\n" => {}
            Ok(other) => {
                fail(&mut report, format!("DDL CREATE {name:?} answered {other:?}"));
                return finish(report, &observer, &clock);
            }
            Err(err) => {
                fail(&mut report, format!("DDL phase: {err}"));
                return finish(report, &observer, &clock);
            }
        }
    }

    // The tier-budget shape's namespaces: tiered `always`, the leg's
    // window with a tenth of it mutable, every generated value inline
    // (the threshold is above the generator's largest value, so the tier
    // class, not the blob class, carries the bytes), the shape's
    // `MAINTAIN-SLICE`.
    if let Some(shape) = scenario.tier_budget {
        for index in shape.namespaces.indices() {
            let class = NsClass::TierBudget(index);
            let load = shape.load(index);
            let create: &[&[u8]] = &[
                b"INF.NS",
                b"CREATE",
                class.name(),
                b"MODE",
                b"durable",
                b"FSYNC",
                b"always",
                b"MEM-BUDGET",
                load.mem_budget,
                b"MUTABLE-FRACTION",
                load.mutable_permille,
                b"MAINTAIN-SLICE",
                load.maintain_slice,
                b"BLOB-THRESHOLD",
                b"16kb",
                b"TIER-IO-MODE",
                b"buffered",
            ];
            let reply =
                setup.call(&mut node, &mut rng, &clock, &disk, scenario.step_ns_max, create);
            match reply {
                Ok(Some(ok)) if ok == b"+OK\r\n" => {}
                Ok(other) => {
                    fail(&mut report, format!("DDL CREATE {class:?} answered {other:?}"));
                    return finish(report, &observer, &clock);
                }
                Err(err) => {
                    fail(&mut report, format!("DDL phase: {err}"));
                    return finish(report, &observer, &clock);
                }
            }
        }
    }

    // The epoch-marks shape's namespace: tiered, in its leg's `FSYNC` class
    // and blob threshold, the m4 tiered budget shape (a 3 MiB window, a
    // tenth of it mutable, 1 MiB slices).
    if let Some(leg) = scenario.epoch_marks {
        let class = NsClass::EpochMarks(leg);
        let create: &[&[u8]] = &[
            b"INF.NS",
            b"CREATE",
            class.name(),
            b"MODE",
            b"durable",
            b"FSYNC",
            leg.fsync(),
            b"MEM-BUDGET",
            b"3mb",
            b"MUTABLE-FRACTION",
            b"100",
            b"MAINTAIN-SLICE",
            b"1mb",
            b"BLOB-THRESHOLD",
            leg.blob_threshold(),
            b"TIER-IO-MODE",
            b"buffered",
        ];
        match setup.call(&mut node, &mut rng, &clock, &disk, scenario.step_ns_max, create) {
            Ok(Some(ok)) if ok == b"+OK\r\n" => {}
            Ok(other) => {
                fail(&mut report, format!("DDL CREATE {class:?} answered {other:?}"));
                return finish(report, &observer, &clock);
            }
            Err(err) => {
                fail(&mut report, format!("DDL phase: {err}"));
                return finish(report, &observer, &clock);
            }
        }
    }

    let mut lift_ns = None;
    if scenario.lift_regime {
        match lift_regime_ddl(&mut node, &mut setup, &mut rng, &clock, &disk, scenario, &mut report)
        {
            Ok(ns) => lift_ns = Some(ns),
            Err(what) => {
                fail(&mut report, what);
                return finish(report, &observer, &clock);
            }
        }
    }
    // The residue plant's precondition (batch 21): the index must be live
    // in the prelude life and a checkpoint must carry its sidecar before
    // the prelude cut — a clean restart seeds the declaration (the live
    // DDL fan is S10's), then seed documents and a forced checkpoint.
    if lift_ns.is_some() && scenario.prelude.is_some() {
        drop(setup);
        node = match lift_regime_seed_life(node, &first_life, &disk, &clock, &observer) {
            Ok(node) => node,
            Err(what) => {
                fail(&mut report, what);
                return finish(report, &observer, &clock);
            }
        };
        if let Err(what) =
            lift_regime_seed_checkpoint(&mut node, &mut rng, &clock, &disk, scenario, &mut report)
        {
            fail(&mut report, what);
            return finish(report, &observer, &clock);
        }
    }

    if scenario.recycle_open_fault {
        // The sim node runs every cell on this thread (the registry is
        // thread-local): the first pool reuse in any cell fires.
        inf_foundation::fault::arm(inf_log::fault::RECYCLE_OPEN_FAIL, FaultSpec::Nth(1));
    }

    // ---- writers -----------------------------------------------------
    let mut writers = Vec::new();
    let classes = [
        (NsClass::Always, scenario.always_writers),
        (NsClass::Everysec, scenario.esec_writers),
        (NsClass::Memory, scenario.mem_writers),
    ];
    let mut id = 0usize;
    for (class, count) in classes {
        for _ in 0..count {
            let cell = (rng.next_u64() % u64::from(scenario.cells)) as usize;
            let fd = node.nets[cell].borrow_mut().connect();
            let writer = Writer::new(
                id,
                cell,
                fd,
                class,
                scenario.seed,
                scenario.prelude.map_or(scenario.ops_per_writer, |p| p.ops_per_writer),
                class != NsClass::Memory,
                0,
            );
            if writer.setup {
                node.nets[cell]
                    .borrow_mut()
                    .client_send(fd, &encode(&[b"INF.NS", b"USE", class.name()]));
            }
            writers.push(writer);
            id += 1;
        }
    }
    if let Some(shape) = scenario.tier_budget {
        for index in shape.namespaces.indices() {
            let class = NsClass::TierBudget(index);
            let load = shape.load(index);
            for _ in 0..load.writers {
                let cell = (rng.next_u64() % u64::from(scenario.cells)) as usize;
                let fd = node.nets[cell].borrow_mut().connect();
                let writer =
                    Writer::new(id, cell, fd, class, scenario.seed, load.ops_per_writer, true, 0);
                node.nets[cell]
                    .borrow_mut()
                    .client_send(fd, &encode(&[b"INF.NS", b"USE", class.name()]));
                writers.push(writer);
                id += 1;
            }
        }
    }

    if let Some(leg) = scenario.epoch_marks {
        let class = NsClass::EpochMarks(leg);
        for _ in 0..scenario.epoch_marks_writers() {
            let cell = (rng.next_u64() % u64::from(scenario.cells)) as usize;
            let fd = node.nets[cell].borrow_mut().connect();
            let writer =
                Writer::new(id, cell, fd, class, scenario.seed, scenario.ops_per_writer, true, 0);
            node.nets[cell]
                .borrow_mut()
                .client_send(fd, &encode(&[b"INF.NS", b"USE", class.name()]));
            writers.push(writer);
            id += 1;
        }
    }

    // ---- the transition prelude (ADR-0086 D4 as amended) ---------------
    // One life in the other barrier class, cut **dirty** at a seeded step
    // inside its traffic window — the shape that reopens a data-bearing
    // tail: a torn tail truncates at the last valid frame's end (a v2
    // frame's unaligned end under FLUSH), MAINTAIN's empty next segment is
    // removed with the residue, and the next life's rotor resumes *there*.
    // (A clean quiesce leaves the empty next segment as the tail and the
    // transition never touches packed data.) The prelude's ledgers are
    // audited against this cut at the transition boot, then rebased to
    // the recovered state, so the final audit binds the main life's cut
    // with the recovered prefix required.
    if scenario.prelude.is_some() {
        assert_eq!(scenario.workload, DurableWorkload::KeyValue, "prelude is a KV-only shape");
        let prelude_ops: u64 = writers.iter().map(|w| w.quota).sum();
        let prelude_cut = 100 + rng.next_below(prelude_ops * 6);
        for _ in 0..prelude_cut {
            report.scheduler_steps += 1;
            if let Err(err) = node.step(&mut rng, &clock, &disk, scenario.step_ns_max) {
                fail(&mut report, format!("prelude phase: {err}"));
                return finish(report, &observer, &clock);
            }
            for writer in &mut writers {
                let mut net = node.nets[writer.cell].borrow_mut();
                let bytes = net.client_recv(writer.fd);
                writer.rx.extend_from_slice(&bytes);
                while let Some(n) = reply_len(&writer.rx) {
                    let reply: Vec<u8> = writer.rx.drain(..n).collect();
                    writer.absorb_reply(reply, clock.now(), scenario.seed, &mut report);
                }
                if writer.setup || writer.inflight.is_some() || writer.sent >= writer.quota {
                    continue;
                }
                let (wire, pending) = writer.next_command(scenario);
                if pending.mutates {
                    writer.ledger.entry(pending.key.clone()).or_default().push(OpRec {
                        state_after: pending.state_after.clone(),
                        sent_at: clock.now(),
                        acked_at: None,
                    });
                }
                writer.inflight = Some(pending);
                net.client_send(writer.fd, &wire);
                writer.sent += 1;
            }
        }
        let prelude_cut_time = clock.now();
        drop(node);
        observer.power_cut(&disk, clock.now(), scenario.seed ^ 0x0FF5_EED2);
        // The residue plant (batch 21): the lift shape on every cell,
        // between the cut and the boot that must lift past it.
        let mut planted = Vec::new();
        if let Some(ns) = lift_ns {
            match crate::lift::plant_lifted_tail(
                &disk,
                Path::new("node"),
                scenario.cells,
                u64::from(scenario.segment_bytes),
                ns,
            ) {
                Ok((cells, skipped)) => {
                    for what in skipped {
                        eprintln!("lift plant skipped: {what}");
                    }
                    report.lift_plants += cells.len() as u64;
                    planted = cells;
                }
                Err(what) => {
                    fail(&mut report, format!("lift plant: {what}"));
                    return finish(report, &observer, &clock);
                }
            }
        }
        // N17 (batch 35): without the lift plant the FLUSH prelude's cut
        // lands after its traffic drained and its frames are sub-sector,
        // so nothing tears, the immediately preallocated empty next
        // segment survives, and recovery resumes there at offset 0 — a
        // class change at a segment boundary, never the packed reopen
        // ADR-0086 D4 reasons about. The torn-tail plant (the sim's own
        // sector-granular cut of a multi-sector frame) makes every
        // FLUSH → FUA seed cross the transition on packed data.
        let torn_ns = NsId(1);
        if first_life.io_mode == SegmentIoMode::Buffered
            && scenario.io_mode == SegmentIoMode::Direct
            && lift_ns.is_none()
        {
            match crate::lift::plant_torn_tail(&disk, Path::new("node"), scenario.cells, torn_ns) {
                Ok(cells) => report.torn_plants += cells.len() as u64,
                Err(what) => {
                    fail(&mut report, format!("torn-tail plant: {what}"));
                    return finish(report, &observer, &clock);
                }
            }
        }
        node = match boot(scenario, PathBuf::from("node"), &disk, &clock, &observer) {
            Ok(node) => node,
            Err(err) => {
                fail(&mut report, format!("transition boot refused: {err}"));
                return finish(report, &observer, &clock);
            }
        };
        let mut steps = 0u64;
        while !node.ready() {
            steps += 1;
            report.scheduler_steps += 1;
            if let Err(err) = node.step(&mut rng, &clock, &disk, scenario.step_ns_max) {
                // A taxonomy refusal at the transition boot is a finding
                // here, not a legal outcome: the transition must never
                // turn a recoverable image into a refused one.
                fail(&mut report, format!("transition boot failed: {err}"));
                return finish(report, &observer, &clock);
            }
            if steps > STALL_STEPS {
                report.stalled = true;
                fail(&mut report, "recovery stalled on the transition boot".to_owned());
                return finish(report, &observer, &clock);
            }
        }
        // The transition's observable: packed tails reopened packed
        // (FLUSH → FUA); FUA → FLUSH reopens at an aligned v3 end.
        for cell in 0..usize::from(scenario.cells) {
            if let Some(stats) = node.plane(cell).durable_stats() {
                report.reopened_packed_tails += stats.reopened_packed_tails;
            }
        }
        // Engagement (batch 34, widened in batch 35): every FLUSH → FUA
        // transition boot reopens one packed tail per cell — the torn
        // prelude tail on the plain half, the lift plant's life-2
        // segment on the lift half (ADR-0086 D4 as amended: ≥ 1 per cell
        // with a v2 tail).
        let reopened = report.reopened_packed_tails;
        if first_life.io_mode == SegmentIoMode::Buffered
            && scenario.io_mode == SegmentIoMode::Direct
            && reopened < u64::from(scenario.cells)
        {
            fail(
                &mut report,
                format!(
                    "TRANSITION ARM VACUOUS seed {:#x}: FLUSH → FUA (K = {}) reopened {reopened} \
                     packed tails on {} cells",
                    scenario.seed, scenario.frames_in_flight, scenario.cells
                ),
            );
        }
        // The lift plant is the arm: a regime whose plant skipped every
        // cell asserts nothing (the skips were only narrated before).
        if lift_ns.is_some() && planted.is_empty() {
            fail(
                &mut report,
                format!(
                    "LIFT PLANT VACUOUS seed {:#x}: the lift regime planted no cell",
                    scenario.seed
                ),
            );
        }
        // The plant's oracle: every planted cell lifted, its sidecar
        // loaded, the tree equals the truth (the planted document
        // included), the lifted records serve, the residue never does.
        if !planted.is_empty() {
            lift_plant_oracle(&mut node, &planted, &mut rng, &clock, &disk, scenario, &mut report);
        }
        // Every writer's in-flight op is now unacked forever (the cut ate
        // the reply path): the ledger already holds it with `acked_at:
        // None`, which is exactly what the audit's admissible set expects.
        for writer in &mut writers {
            writer.inflight = None;
        }
        if audit_ledgers(
            &mut node,
            &mut writers,
            &mut rng,
            &clock,
            &disk,
            scenario,
            prelude_cut_time,
            Some(clock.now()),
            &mut report,
        )
        .is_err()
        {
            return finish(report, &observer, &clock);
        }
        for writer in &mut writers {
            writer.reconnect(&mut node, scenario.ops_per_writer);
        }
        // The lift regime's writers join the main life only: their records
        // are exactly what the final boot lifts past the prelude's residue.
        if scenario.lift_regime {
            for class in [NsClass::Tiered, NsClass::Tiered, NsClass::Indexed, NsClass::Indexed] {
                let cell = (rng.next_u64() % u64::from(scenario.cells)) as usize;
                let fd = node.nets[cell].borrow_mut().connect();
                let writer = Writer::new(
                    id,
                    cell,
                    fd,
                    class,
                    scenario.seed,
                    scenario.ops_per_writer,
                    true,
                    0,
                );
                node.nets[cell]
                    .borrow_mut()
                    .client_send(fd, &encode(&[b"INF.NS", b"USE", class.name()]));
                writers.push(writer);
                id += 1;
            }
        }
    }

    // ---- traffic until the seeded cut point ---------------------------
    // The cut lands somewhere inside (or just past) the traffic window so
    // every pipeline stage — staged, framed, written, fsynced, acked,
    // checkpoint/manifest mid-swap — gets cut across the seed corpus.
    let total_ops: u64 = writers.iter().map(|w| w.quota).sum();
    let cut_step = 200 + rng.next_below(total_ops * 6);
    let cut_step = scenario.epoch_marks.map_or(cut_step, |leg| leg.cut_step(cut_step));
    // The ledger-wedge leg cuts on its own reading (`LedgerWedge::cut_due`),
    // at the seeded step or after the held seal's fold.
    let seeded_cut = cut_step;
    let mut ledger_wedge = scenario.ledger_wedge.map(ledger_wedge::LedgerWedge::new);
    let cut_step = if ledger_wedge.is_some() { u64::MAX } else { cut_step };
    // M3-S23: two seeded mid-run equivalence instants. Each quiesces
    // (drain in-flight, send nothing new), compares live state against a
    // read-only shadow replay, then resumes. The cut itself is never
    // quiesced — draining before it would erase the unacked-tail cases
    // the durability oracle exists for (ADR-0045 D1).
    let document_workload = scenario.workload == DurableWorkload::Document;
    let mut equivalence = crate::document::EquivalenceStats::default();
    let checks_at = [cut_step / 3, cut_step / 3 * 2];
    let mut next_check = if document_workload { 0 } else { checks_at.len() };
    let mut idle_steps = 0u64;
    let mut last_progress = 0u64;
    let mut log_oracle = LogOracle::new(scenario.cells);
    let mut tier_oracle = TierTurnOracle::new(&node, scenario.cells, true);
    let no_model_leg = scenario.tier_budget.is_some_and(|shape| shape.leg.model_absent());
    if no_model_leg {
        tier_oracle.arm_after_the_visits_bound();
    }
    let mut epilogue_sent = false;
    let mut marks_cut =
        (scenario.epoch_marks == Some(EpochMarksLeg::Wedge)).then(MarksCut::default);
    for step in 0..cut_step {
        report.scheduler_steps += 1;
        if let Err(err) = node.step(&mut rng, &clock, &disk, scenario.step_ns_max) {
            fail(&mut report, format!("traffic phase: {err}"));
            return finish(report, &observer, &clock);
        }
        if marks_cut.as_mut().is_some_and(|cut| cut.due(&node)) {
            break;
        }
        log_oracle.sample(&node, &mut report);
        tier_oracle.sample(&node, &disk, scenario, &mut report);
        if let Some(wedge) = ledger_wedge.as_mut() {
            wedge.observe(step, &node, &disk, clock.now());
            if wedge.cut_due(step, seeded_cut) {
                break;
            }
        }
        let quiesce = next_check < checks_at.len() && step >= checks_at[next_check];
        let mut progress = 0u64;
        for writer in &mut writers {
            let mut net = node.nets[writer.cell].borrow_mut();
            let bytes = net.client_recv(writer.fd);
            progress += bytes.len() as u64;
            writer.rx.extend_from_slice(&bytes);
            while let Some(n) = reply_len(&writer.rx) {
                let reply: Vec<u8> = writer.rx.drain(..n).collect();
                writer.absorb_reply(reply, clock.now(), scenario.seed, &mut report);
            }
            if writer.setup || writer.inflight.is_some() || writer.sent >= writer.quota {
                continue;
            }
            if quiesce {
                // Mid-run oracle instant: drain, don't send.
                continue;
            }
            if clock.now() < writer.idle_until {
                continue;
            }
            let (wire, pending) = writer.next_command(scenario);
            if pending.mutates {
                writer.ledger.entry(pending.key.clone()).or_default().push(OpRec {
                    state_after: pending.state_after.clone(),
                    sent_at: clock.now(),
                    acked_at: None,
                });
            }
            writer.inflight = Some(pending);
            net.client_send(writer.fd, &wire);
            writer.sent += 1;
            progress += 1;
            let think = match writer.class {
                NsClass::Everysec => scenario.esec_think_ns_max,
                NsClass::Always => scenario.always_think_ns_max,
                NsClass::TierBudget(index) => {
                    scenario.tier_budget.map_or(0, |s| s.load(index).think_ns_max)
                }
                NsClass::EpochMarks(leg) => leg.think_ns_max(),
                NsClass::Memory | NsClass::Tiered | NsClass::Indexed => 0,
            };
            if think > 0 {
                writer.idle_until = clock.now() + Nanos(writer.rng.next_below(think));
            }
        }
        // The no-model leg's epilogue: once its tier writers are done, the
        // namespace's mutable target drops to a hundredth of the window,
        // so the next seal step takes the whole 64 MiB slice and the next
        // plan stages a round of more ops than one turn's visits — the
        // regime the leg's oracles read. Sent through a finished tier
        // writer, whose reply loop checks the `+OK`.
        if no_model_leg && !epilogue_sent && tier_fill_done(&writers) {
            epilogue_sent = true;
            let writer = writers
                .iter_mut()
                .find(|w| matches!(w.class, NsClass::TierBudget(_)))
                .expect("the no-model leg has tier writers");
            let wire = encode(&[
                b"INF.NS",
                b"SET",
                NsClass::TierBudget(0).name(),
                b"MUTABLE-FRACTION",
                b"10",
            ]);
            writer.inflight = Some(Pending {
                key: Vec::new(),
                state_after: None,
                expect: b"+OK\r\n".to_vec(),
                mutates: false,
                taints: false,
            });
            node.nets[writer.cell].borrow_mut().client_send(writer.fd, &wire);
            writer.sent += 1;
            writer.quota += 1;
        }
        // The log must be quiescent too (ADR-0087 D7): an `everysec` ack
        // precedes its frame landing, and under the stall model plain
        // writes land later — the shadow replay reads the file, so every
        // sealed frame must have its `LogWritten` and nothing may sit
        // staged behind a bounded wait.
        let log_quiet = (0..usize::from(scenario.cells)).all(|cell| {
            node.plane(cell)
                .durable_stats()
                .is_none_or(|s| s.frames_in_flight_now == 0 && s.records_staged == 0)
        });
        if quiesce && log_quiet && writers.iter().all(|w| !w.setup && w.inflight.is_none()) {
            crate::document::equivalence_check(
                scenario,
                &format!("mid-run-{}", next_check + 1),
                &node,
                &disk,
                clock.now(),
                &mut equivalence,
                &mut report.violations,
            );
            next_check += 1;
        }
        if progress == 0 {
            idle_steps += 1;
            // Quiesced early: idle time still ticks (everysec fsyncs,
            // checkpoint cycles) until the seeded cut arrives.
            if idle_steps >= STALL_STEPS && writers.iter().any(|w| w.replied < w.sent) {
                // The watermark-liveness verdict (M2.5-S14): name the
                // writers stuck behind an unadvancing fsync watermark —
                // "stalled forever behind a stuck fsync" is a finding,
                // not a timeout.
                let stuck: Vec<String> = writers
                    .iter()
                    .filter(|w| w.replied < w.sent)
                    .map(|w| format!("writer {} ({:?})", w.id, w.class))
                    .collect();
                report.stalled = true;
                fail(
                    &mut report,
                    format!(
                        "WATERMARK LIVENESS VIOLATION seed {:#x}: traffic stalled before the \
                         cut with unacked in-flight ops ({})",
                        scenario.seed,
                        stuck.join(", ")
                    ),
                );
                return finish(report, &observer, &clock);
            }
        } else {
            idle_steps = 0;
            last_progress = report.commands_done;
        }
    }
    let _ = last_progress;

    // ---- CLEAN STOP (ADR-0124) ------------------------------------------
    // Every cell is asked to stop; the harness plays the assembly: once
    // every cell is `Quiet` it says `finish_stop`, and steps until every
    // cell is `Drained`. Replies still arriving are absorbed (they are
    // acks the rule will require). `Plant::StopKill` is the pre-fix
    // behaviour — the process dies at the request, no drain.
    if scenario.clean_stop && scenario.plant == Plant::StopKill {
        // The teeth fire here: the cut below is the stop.
        report.plant_fired = true;
    }
    if scenario.clean_stop && scenario.plant != Plant::StopKill {
        let cells = usize::from(scenario.cells);
        for cell in 0..cells {
            node.plane_mut(cell).request_stop();
        }
        let mut steps = 0u64;
        loop {
            let phases: Vec<inf_server::StopPhase> =
                (0..cells).map(|c| node.plane(c).stop_phase()).collect();
            if phases.iter().all(|p| *p == inf_server::StopPhase::Drained) {
                break;
            }
            if phases
                .iter()
                .all(|p| matches!(p, inf_server::StopPhase::Quiet | inf_server::StopPhase::Drained))
            {
                for cell in 0..cells {
                    node.plane_mut(cell).finish_stop();
                }
            }
            steps += 1;
            report.scheduler_steps += 1;
            if let Err(err) = node.step(&mut rng, &clock, &disk, scenario.step_ns_max) {
                fail(&mut report, format!("clean stop: {err}"));
                return finish(report, &observer, &clock);
            }
            for writer in &mut writers {
                let bytes = node.nets[writer.cell].borrow_mut().client_recv(writer.fd);
                writer.rx.extend_from_slice(&bytes);
                while let Some(n) = reply_len(&writer.rx) {
                    let reply: Vec<u8> = writer.rx.drain(..n).collect();
                    writer.absorb_reply(reply, clock.now(), scenario.seed, &mut report);
                }
            }
            if steps > STALL_STEPS {
                report.stalled = true;
                fail(
                    &mut report,
                    format!(
                        "CLEAN STOP LIVENESS VIOLATION seed {:#x}: cells still {phases:?} after    \
                                                   {steps} steps",
                        scenario.seed
                    ),
                );
                return finish(report, &observer, &clock);
            }
        }
        report.clean_stop_steps = steps;
        for writer in &writers {
            if !node.nets[writer.cell].borrow().closed(writer.fd) {
                fail(
                    &mut report,
                    format!(
                        "CLEAN STOP VIOLATION seed {:#x}: writer {} ({:?}) never saw its close",
                        scenario.seed, writer.id, writer.class
                    ),
                );
                return finish(report, &observer, &clock);
            }
        }
    }

    // ---- POWER CUT ----------------------------------------------------
    let cut_time = clock.now();
    if let Some(leg) = scenario.epoch_marks {
        epoch_marks_scrape(scenario, leg, &node, &mut report);
    }
    if let Some(wedge) = &ledger_wedge {
        wedge.judge(scenario.seed, &node, &mut report);
    }
    for cell in 0..usize::from(scenario.cells) {
        if let Some(stats) = node.plane(cell).durable_stats() {
            report.frames_in_flight_max =
                report.frames_in_flight_max.max(stats.frames_in_flight_max);
            report.frame_waits_barrier += stats.frame_waits_barrier;
            report.frame_waits_rotation += stats.frame_waits_rotation;
            report.frame_waits_reorder += stats.frame_waits_reorder;
            if scenario.reorder_oracle && stats.frame_waits_reorder > 0 {
                frame_queue_filled(scenario.seed, cell, &node, &mut report);
            }
            report.frame_waits_ledger += stats.frame_waits_ledger;
            report.frame_waits_ack_map += stats.frame_waits_ack_map;
            report.ledger_prealloc_deferrals += stats.ledger_prealloc_deferrals;
            report.frame_waits_fill += stats.frame_waits_fill;
            report.frame_waits_group += stats.frame_waits_group;
            report.frame_waits_pace += stats.frame_waits_pace;
            report.write_stall_max_us = report.write_stall_max_us.max(stats.write_stall_max_us);
            report.segments_recycled += stats.segments_recycled;
            report.recycle_misses += stats.recycle_misses;
            report.recycle_fallbacks += stats.recycle_fallbacks;
            report.recycle_served += served_preallocs(&stats);
            report.recycle_sentinels += stats.recycle_sentinels;
            report.segment_rotations += stats.segment_rotations;
            report.recycle_waits_started += stats.recycle_waits_started;
            report.recycle_waits_satisfied += stats.recycle_waits_satisfied;
            report.recycle_waits_expired += stats.recycle_waits_expired;
            report.segment_inline_preallocs += stats.segment_inline_preallocs;
            // ADR-0090 A8: every wait ends exactly once, and a wait never
            // strands a rotation without a next segment.
            if stats.recycle_waits_started
                < stats.recycle_waits_satisfied + stats.recycle_waits_expired
            {
                fail(
                    &mut report,
                    format!(
                        "POOL-WAIT ACCOUNTING VIOLATION seed {:#x} cell {cell}: started {} < \
                         satisfied {} + expired {}",
                        scenario.seed,
                        stats.recycle_waits_started,
                        stats.recycle_waits_satisfied,
                        stats.recycle_waits_expired
                    ),
                );
            }
            if scenario.recycle_oracle
                && scenario.prealloc != inf_server::PreallocPolicy::Immediate
                && stats.segment_inline_preallocs > 0
            {
                fail(
                    &mut report,
                    format!(
                        "POOL WAIT STRANDED A ROTATION seed {:#x} cell {cell}: {} inline \
                         preallocs under {:?}",
                        scenario.seed, stats.segment_inline_preallocs, scenario.prealloc
                    ),
                );
            }
            // ADR-0090 D5, the recycle oracle (per cell, at the cut).
            if scenario.recycle_oracle
                && scenario.recycle_slots > 0
                && scenario.io_mode == SegmentIoMode::Direct
            {
                let truncated = stats.segments_truncated;
                // ADR-0090 A16 (batch 71): the rule's precondition is the
                // pool's state, not a rotation count — seed 0xc10005 rotated
                // 3 times in a burst whose checkpoints trailed every
                // prealloc, so the pool was fed only after the last one.
                // A prealloc that found the pool non-empty (not a miss, not
                // a space failure) must have recycled or fallen back.
                let served = served_preallocs(&stats);
                if served > 0 && stats.segments_recycled == 0 && stats.recycle_fallbacks == 0 {
                    fail(
                        &mut report,
                        format!(
                            "RECYCLING NEVER ENGAGED seed {:#x} cell {cell}: {} preallocs found \
                             the pool non-empty ({} preallocs, {} misses, {} space failures), \
                             0 recycled, 0 fallbacks ({} rotations, {} truncations)",
                            scenario.seed,
                            served,
                            stats.segment_preallocs,
                            stats.recycle_misses,
                            stats.segment_prealloc_failures,
                            stats.segment_rotations,
                            truncated
                        ),
                    );
                }
                // The feed half: two covered pre-zeroed truncations and the
                // pool never held a segment (taken, refused full, or held
                // at the cut) means truncation stopped offering (D1).
                let pool_saw_a_segment = stats.segments_recycled
                    + stats.recycle_fallbacks
                    + stats.recycle_pool_full
                    + u64::from(stats.recycle_pool_bytes > 0)
                    > 0;
                if truncated >= 2
                    && stats.rotations_unzeroed == 0
                    && !scenario.recycle_open_fault
                    && !pool_saw_a_segment
                {
                    fail(
                        &mut report,
                        format!(
                            "TRUNCATIONS NEVER FED THE POOL seed {:#x} cell {cell}: {} \
                             truncations, pool never held a segment ({} rotations, {} misses)",
                            scenario.seed, truncated, stats.segment_rotations, stats.recycle_misses
                        ),
                    );
                }
                let unserved = stats.segment_preallocs.saturating_sub(stats.segments_recycled);
                let bound = unserved * u64::from(scenario.segment_bytes);
                if stats.zero_fill_bytes > bound {
                    fail(
                        &mut report,
                        format!(
                            "ZERO-FILL ACCOUNTING VIOLATION seed {:#x} cell {cell}: \
                            zero_fill_bytes \
                             {} > (preallocs {} − recycled {}) × segment_bytes {}",
                            scenario.seed,
                            stats.zero_fill_bytes,
                            stats.segment_preallocs,
                            stats.segments_recycled,
                            scenario.segment_bytes
                        ),
                    );
                }
            }
            for class in inf_runtime::IoClass::ALL {
                let c = stats.io_budget[class.index()];
                if !class.is_foreground() {
                    report.budget_background_bytes += c.spent_bytes;
                }
                report.budget_deferrals += c.deferrals;
                report.budget_unattainable += c.unattainable;
            }
            report.ckpt_block_wait_ns_max =
                report.ckpt_block_wait_ns_max.max(stats.ckpt_block_wait_ns_max);
        }
    }
    if let Some(shape) = scenario.tier_budget {
        tier_budget_scrape(scenario, shape, &node, &disk, &mut report);
    }
    derived_ledger_counters(scenario.seed, &mut report);
    engagement_checks(scenario, &node, &mut report);
    if scenario.budget_oracle {
        budget_oracles(scenario, &node, clock.now(), &mut report);
        overrun_oracles(scenario, &node, &mut report);
    }
    if scenario.ckpt_direct_refused_after.is_some() {
        for cell in 0..usize::from(scenario.cells) {
            let (downgrades, frame_bytes) = node
                .plane(cell)
                .durable_stats()
                .map_or((0, 0), |s| (s.ckpt_io_mode_downgrades, s.log_frame_bytes));
            let (completed, aborted) = node.plane(cell).ckpt_stats_for_sim();
            report.ckpt_downgrades += downgrades;
            // A cell cut before four intervals of frames may not have
            // reached its retry's publish (one refused attempt, the
            // immediate retry, a few slices of walk): not a verdict.
            // The manifest discloses how many cells exercised the
            // downgrade (`ckpt_downgrades`), so a sweep that never did
            // is visible.
            if frame_bytes < 4 * scenario.ckpt_interval_bytes {
                continue;
            }
            if downgrades != 1 || completed == 0 {
                fail(
                    &mut report,
                    format!(
                        "CHECKPOINT LIVENESS VIOLATION seed {:#x} cell {cell}: direct writes \
                         refused after the probe — downgrades {downgrades} (want 1), \
                         checkpoints completed {completed} (want ≥ 1), aborted {aborted}, \
                         log frame bytes {frame_bytes}",
                        scenario.seed
                    ),
                );
            }
        }
    }
    // Batch 42 (F-L01-01): a tick that fired over staged records and
    // counted itself idle dropped its due — the records wait a second.
    if report.idle_tick_violations > 0 {
        let what = format!(
            "EVERYSEC TICK IDLE OVER STAGED RECORDS seed {:#x}: {} tick(s) counted idle \
             while records sat in the staging builder (waits_fill {}, waits_group {})",
            scenario.seed,
            report.idle_tick_violations,
            report.frame_waits_fill,
            report.frame_waits_group
        );
        fail(&mut report, what);
    }
    // Batch 43 (F-L01-04): an open hold-episode clock with nothing held
    // is a stale clock — the next episode's first sight reads as elapsed.
    if report.hold_episode_violations > 0 {
        let what = format!(
            "HOLD EPISODE CLOCK OPEN WITH NOTHING HELD seed {:#x}: {} sample(s) with a fill or \
             group-hold clock open and no frame or standalone held (waits_fill {}, waits_group {})",
            scenario.seed,
            report.hold_episode_violations,
            report.frame_waits_fill,
            report.frame_waits_group
        );
        fail(&mut report, what);
    }
    // Batch 42 (F-L01-03): the LSN→seq ack map holds at most the frames
    // behind the written prefix, one entry per ledger coverage point,
    // and one more — never one per frame sealed behind a stalled barrier.
    let map_bound = REORDER_WINDOW_FRAMES as u64 + report.fsync_entries_max + 1;
    if report.frames_awaiting_max > map_bound {
        let what = format!(
            "ACK MAP UNBOUNDED seed {:#x}: frames awaiting the watermark peaked at {} \
             against a bound of {} (reorder window {} + ledger entries {} + 1)",
            scenario.seed,
            report.frames_awaiting_max,
            map_bound,
            REORDER_WINDOW_FRAMES,
            report.fsync_entries_max
        );
        fail(&mut report, what);
    }
    // Batch 44 (ADR-0087 D2 third amendment): write-through tickets
    // behind a wedged front never outgrow the window — one per frame
    // before, bounded only by the gated clients' outstanding commands.
    if report.write_through_entries_max > WRITE_THROUGH_WINDOW_ENTRIES as u64 {
        let what = format!(
            "WRITE-THROUGH TICKETS UNBOUNDED seed {:#x}: unfolded write-through tickets \
             peaked at {} against the window of {}",
            scenario.seed, report.write_through_entries_max, WRITE_THROUGH_WINDOW_ENTRIES
        );
        fail(&mut report, what);
    }
    // M4.5-S39a: on the aligned class every frame of the everysec-only
    // reorder shape is barrier-less and far below the target — the
    // policy must have held at least once, or the arm measured nothing.
    if scenario.reorder_oracle
        && scenario.fill.enabled()
        && scenario.io_mode == SegmentIoMode::Direct
        && report.frame_waits_fill == 0
    {
        fail(
            &mut report,
            format!(
                "FILL POLICY NOT ENGAGED seed {:#x}: the aligned everysec-only shape never held \
                 a frame",
                scenario.seed
            ),
        );
    }
    if scenario.reorder_oracle && report.frame_waits_reorder == 0 {
        let depth = report.frames_in_flight_max;
        fail(
            &mut report,
            format!(
                "VACUOUS seed {:#x}: frame-queue-unengaged — the wedged device never filled the \
                 completion ledger's frame queue (frames_in_flight_max {depth}): the scenario \
                 proves nothing about the bound",
                scenario.seed
            ),
        );
    }
    drop(node); // the process dies: in-flight state vanishes
    observer.power_cut(&disk, clock.now(), scenario.seed ^ 0x0FF5_EED0);
    if document_workload {
        // M3-S24 (ADR-0045 D4): disclose which record class the surviving
        // image ends on — cut coverage is measured, never assumed.
        report.cut_classes =
            crate::document::classify_cut(&disk, &PathBuf::from("node"), scenario.cells);
    }

    // ---- reboot (+ optional second cut mid-recovery) -------------------
    let mut boots = 0;
    let node = loop {
        boots += 1;
        let mut node = match boot(scenario, PathBuf::from("node"), &disk, &clock, &observer) {
            Ok(node) => node,
            Err(err) => {
                fail(&mut report, format!("reboot {boots} refused: {err}"));
                return finish(report, &observer, &clock);
            }
        };
        let double = scenario.double_cut && boots == 1;
        let recovery_budget = if double { 1 + rng.next_below(200) } else { u64::MAX };
        let mut steps = 0u64;
        let mut failed = None;
        while !node.ready() && steps < recovery_budget {
            steps += 1;
            report.scheduler_steps += 1;
            if let Err(err) = node.step(&mut rng, &clock, &disk, scenario.step_ns_max) {
                failed = Some(err);
                break;
            }
            if steps > STALL_STEPS {
                report.stalled = true;
                fail(&mut report, format!("recovery stalled on boot {boots}"));
                return finish(report, &observer, &clock);
            }
        }
        if let Some(err) = failed {
            // The ADR-0018 taxonomy refusal is a LEGAL outcome: interior
            // data beyond lost un-fsynced bytes fail-stops the boot
            // (never silent truncation). But §8.2 binds SURVIVAL, not
            // serving: acked data must still exist in the surviving
            // image — audited directly, so an ack-ahead-of-durability
            // bug (the canary) cannot hide behind the refusal.
            if err.to_string().contains("log corruption") {
                if scenario.workload == DurableWorkload::Document {
                    fail(
                        &mut report,
                        format!(
                            "DOCUMENT RECOVERY VIOLATION seed {:#x}: honest power-cut image \
                             refused boot: {err}",
                            scenario.seed
                        ),
                    );
                    return finish(report, &observer, &clock);
                }
                report.refused_boot = true;
                // ADR-0090 D5: on the recycling scenario a refusal *is*
                // the refusal failure mode the residue rule exists to
                // close (residue taken for a seq gap or a hole) — a
                // finding, not a legal outcome. The planted-bug canary
                // (`--cfg inf_canary_foreign_segment`) must turn this red.
                if scenario.recycle_oracle {
                    fail(
                        &mut report,
                        format!(
                            "RECYCLED RESIDUE REFUSED seed {:#x}: honest power-cut image of a \
                             recycling log refused boot: {err}",
                            scenario.seed
                        ),
                    );
                    return finish(report, &observer, &clock);
                }
                // Refusals are counted in the sweep manifest; the *class*
                // must be visible too (§8.4 never-silent, M2.5-S12: the
                // residual-refusal taxonomy after ADR-0031 is a ledger
                // observable).
                eprintln!("refused boot {boots}: {err}");
                let mut tally = AuditTally::default();
                survival_audit(scenario, &disk, &writers, cut_time, &mut tally);
                report.required_ops += tally.required_ops;
                report.allowed_lost_ops += tally.allowed_lost_ops;
                report.audited_keys += tally.audited_keys;
                report.violations.extend(tally.violations);
            } else {
                fail(&mut report, format!("recovery failed on boot {boots}: {err}"));
            }
            return finish(report, &observer, &clock);
        }
        if node.ready() {
            break node;
        }
        // The second cut: recovery itself was interrupted (idempotence).
        drop(node);
        observer.power_cut(&disk, clock.now(), scenario.seed ^ 0x0FF5_EED1 ^ boots);
    };
    let mut node = node;
    // The recovered life's first turns: every tiered namespace whose
    // replayed tail is past its mutable target seals a slice in its first
    // MAINTAIN, before that turn's plan, so on the eight-namespace leg all
    // eight are idle with work in one turn — the one-plan oracle's regime.
    if scenario.tier_budget.is_some()
        && let Err(err) =
            tier_recovered_turns(&mut node, &mut rng, &clock, &disk, scenario, &mut report)
    {
        fail(&mut report, format!("the recovered life's first turns: {err}"));
        return finish(report, &observer, &clock);
    }
    // ADR-0124 D4's observable: a boot after a clean stop replays no tail
    // record on any cell — the stop checkpoint covered everything.
    if scenario.clean_stop && scenario.plant != Plant::StopKill {
        for cell in 0..usize::from(scenario.cells) {
            let mut probe = MiniClient::connect(&mut node, cell);
            let reply = probe.call(
                &mut node,
                &mut rng,
                &clock,
                &disk,
                scenario.step_ns_max,
                &[b"INFO", b"persistence"],
            );
            let Ok(Some(text)) = reply else {
                fail(&mut report, format!("clean stop: INFO on cell {cell} answered {reply:?}"));
                return finish(report, &observer, &clock);
            };
            let text = String::from_utf8_lossy(&text);
            let replayed: u64 = text
                .lines()
                .find_map(|l| l.strip_prefix("recover_replay_records:"))
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(u64::MAX);
            report.clean_stop_replay_records += replayed;
            if replayed != 0 {
                fail(
                    &mut report,
                    format!(
                        "CLEAN STOP REPLAY VIOLATION seed {:#x}: cell {cell} replayed {replayed} \
                         tail records after a clean stop (no stop checkpoint covered them)",
                        scenario.seed
                    ),
                );
                return finish(report, &observer, &clock);
            }
        }
    }
    // ADR-0090 D4: what the reboot proved about recycled residue — the
    // sweep's coverage disclosure (a sweep whose reboots never met
    // residue never exercised the rule).
    for cell in 0..usize::from(scenario.cells) {
        let residue = node.control.recovery_board().slot(cell as u16).residue();
        report.recycled_residue_slacks += residue.recycled_residue_slacks;
        report.stale_residue_slacks += residue.stale_residue_slacks;
    }
    // ADR-0090 A15 (review 2026-08-30 F-L02-02): a boot after a clean stop
    // never reports a torn tail — the stop drained every frame, so a
    // torn verdict can only be residue misread as this life's garbage
    // (the data end inside the last residue frame's body).
    if scenario.clean_stop && scenario.plant != Plant::StopKill {
        for cell in 0..usize::from(scenario.cells) {
            let slot = node.control.recovery_board().slot(cell as u16);
            if let Some(at) = slot.torn_truncated_at() {
                report.clean_stop_torn_tails += 1;
                fail(
                    &mut report,
                    format!(
                        "PHANTOM TORN TAIL seed {:#x}: cell {cell} booted after a clean stop \
                         with torn_truncated_at = {at} ({} recycled residue slacks)",
                        scenario.seed,
                        slot.residue().recycled_residue_slacks
                    ),
                );
            }
        }
    }
    if scenario.lift_regime {
        for writer in &writers {
            let acked =
                writer.ledger.values().flatten().filter(|op| op.acked_at.is_some()).count() as u64;
            match writer.class {
                NsClass::Tiered => report.lift_tiered_ops += acked,
                NsClass::Indexed => report.lift_indexed_ops += acked,
                NsClass::Always
                | NsClass::Everysec
                | NsClass::Memory
                | NsClass::TierBudget(_)
                | NsClass::EpochMarks(_) => {}
            }
        }
        for cell in 0..usize::from(scenario.cells) {
            report.lift_sidecars_loaded +=
                u64::from(node.plane(cell).keyspace().idx_sidecar_info().loaded);
        }
        lift_regime_index_oracle(
            &mut node,
            &mut rng,
            &clock,
            &disk,
            scenario,
            "after the final boot",
            &mut report,
        );
    }

    // ---- the "at end" equivalence check (M3-S23) -----------------------
    // Runs post-recovery by design: recovered live state must equal an
    // independent replay of the post-cut disk. Quiescing *before* the cut
    // instead would erase the unacked-tail durability cases (ADR-0045 D1).
    if document_workload {
        crate::document::equivalence_check(
            scenario,
            "post-recovery",
            &node,
            &disk,
            clock.now(),
            &mut equivalence,
            &mut report.violations,
        );
    }
    report.equivalence_checks = equivalence.checks;
    report.documents_compared = equivalence.documents_compared;
    report.corpus_documents_used = writers.iter().map(|w| w.corpus_docs_used).sum();

    // ---- audit ----------------------------------------------------------
    if audit_ledgers(
        &mut node,
        &mut writers,
        &mut rng,
        &clock,
        &disk,
        scenario,
        cut_time,
        None,
        &mut report,
    )
    .is_err()
    {
        return finish(report, &observer, &clock);
    }
    if scenario.recycle_open_fault {
        let fired = inf_foundation::fault::fired(inf_log::fault::RECYCLE_OPEN_FAIL);
        inf_foundation::fault::disarm(inf_log::fault::RECYCLE_OPEN_FAIL);
        // ADR-0090 A17 (F-L19-20): the arm's precondition is A16's — a
        // prealloc that found the pool non-empty is the take the point
        // intercepts. The point stays armed through the recovered life,
        // so that life's takes and fallbacks count too (sampled here, on
        // the audited node). A seed whose lives never reached the pool
        // (seed 0xd5ee0103: 6 preallocs, 6 misses) is unreached, disclosed.
        let mut fallbacks = report.recycle_fallbacks;
        for cell in 0..usize::from(scenario.cells) {
            if let Some(stats) = node.plane(cell).durable_stats() {
                report.recycle_served += served_preallocs(&stats);
                fallbacks += stats.recycle_fallbacks;
            }
        }
        match open_fault_verdict(scenario.seed, report.recycle_served, fired, fallbacks) {
            OpenFaultVerdict::Ok => {}
            OpenFaultVerdict::Unreached => report.recycle_open_unreached = true,
            OpenFaultVerdict::Violation(what) => fail(&mut report, what),
        }
    }

    finish(report, &observer, &clock)
}

/// The frame queue's cap on a cell that held a frame for it: the hold
/// answers only at the queue's own admission, so the queue's census row
/// read its cap. A row short of it held the frame early (token
/// `frame-queue-unfilled`).
fn frame_queue_filled(seed: u64, cell: usize, node: &Node, report: &mut DurableReport) {
    let name = inf_log::limits::FRAME_QUEUE.name();
    let census = node.plane(cell).cap_census();
    let Some(row) = census.row(name) else {
        let what = format!(
            "REORDER WINDOW VIOLATION seed {seed:#x} cell {cell}: frame-queue-unfilled — a frame \
             held for the queue and no `{name}` row"
        );
        report.violations.push(what);
        return;
    };
    if row.high_water < row.entries_max {
        let what = format!(
            "REORDER WINDOW VIOLATION seed {seed:#x} cell {cell}: frame-queue-unfilled — a frame \
             held for the queue at a high water of {} of its {}",
            row.high_water, row.entries_max
        );
        report.violations.push(what);
    }
}

/// The fsync ledger's derived counters, read 0 on every seed: the ack map
/// holds at most the frame queue, the ledger and one point, which is its
/// cap, so no frame waits for it; and the rotation class holds at most two
/// prealloc and two zero-fill barriers (a seal, then the barriers of the two
/// generations after it), so no MAINTAIN skips one. A nonzero count is a
/// derivation the run disproved.
fn derived_ledger_counters(seed: u64, report: &mut DurableReport) {
    if report.frame_waits_ack_map > 0 {
        let what = format!(
            "LEDGER DERIVATION VIOLATION seed {seed:#x}: ack-map-held — {} frame hold(s) on a \
             full ack map",
            report.frame_waits_ack_map
        );
        report.violations.push(what);
    }
    if report.ledger_prealloc_deferrals > 0 {
        let what = format!(
            "LEDGER DERIVATION VIOLATION seed {seed:#x}: ledger-prealloc-deferred — {} prealloc \
             or zero-fill barrier(s) skipped at the rotation class's budget",
            report.ledger_prealloc_deferrals
        );
        report.violations.push(what);
    }
}

/// Preallocs that found the pool non-empty — neither a miss nor a space
/// failure (ADR-0090 A16): the takes the recycle rules are about.
/// The tier-budget shape's per-cell scrape at the cut (before the node
/// drops: the counters are life-scoped), and the tier class's oracles on
/// it. A cell whose budget model is absent, whose tier class never made
/// an offer, or on which a tier-budget namespace never advanced its
/// flush watermark is `VACUOUS`: the shape exists to flush tiered
/// namespaces under the model, and a run that did not is no evidence
/// about the class. The oracles, each independent of the budget's code:
/// no offer above the class cap and no overrun (the counters), one
/// driver entry's tier bytes within the cap and the pool computed from
/// the model (the driver's own tally), every ready group's wait within
/// the fair turn's bound `n × T_c` computed from the model and the
/// scenario's step (the plane's gauge, judged by the harness's
/// arithmetic), the spend equal to the disk's tier bytes and ops with the
/// ops a cut caught between push and submit (exact), and the leg's own
/// reads: contention and refusals on the legs that reach them, a plan
/// passed over on the eight-namespace leg, neither on the control leg;
/// with the write-fault plant, a retry.
fn tier_budget_scrape(
    scenario: &DurableScenario,
    shape: TierBudgetShape,
    node: &Node,
    disk: &SimDisk,
    report: &mut DurableReport,
) {
    let bound_ns = fair_turn_bound_ns(
        scenario.device.model_share,
        u64::from(shape.namespaces.get()),
        scenario.step_ns_max,
    );
    let pass_bound = tier_pass_bound_bytes(scenario.device.model_share);
    if scenario.plant == crate::net::Plant::TierWriteEio {
        report.plant_fired |= disk.faults_fired() > 0;
    }
    for cell in 0..usize::from(scenario.cells) {
        let Some(stats) = node.plane(cell).durable_stats() else {
            report.violations.push(format!(
                "TIER BUDGET VACUOUS seed {:#x} cell {cell}: no durable plane",
                scenario.seed
            ));
            continue;
        };
        let record = tier_budget_cell(node, cell, shape, &stats);
        tier_budget_vacuous(scenario, shape, stats.io_budget_model_absent, &record, report);
        tier_budget_oracles(scenario, shape.leg, bound_ns, pass_bound, &record, report);
        report.tier_budget_cells.push(record);
    }
}

/// One cell's tier class at the cut: the budget's counters, the driver's
/// observed and queued tier I/O, the turn's gauges and each namespace's
/// watermarks.
fn tier_budget_cell(
    node: &Node,
    cell: usize,
    shape: TierBudgetShape,
    stats: &inf_server::DurableStats,
) -> TierBudgetCell {
    use inf_runtime::IoClass;
    use inf_server::TierFlushGauge as G;
    let counted = stats.io_budget[IoClass::TierFlush.index()];
    let observed = node.cells[cell].0.driver().observed_io();
    let (queued_bytes, queued_ops) = node.cells[cell].0.driver().queued_tier_io();
    let gauges = node.plane(cell).tier_flush_gauges();
    let gauge = |slot: G| gauges[slot as usize];
    TierBudgetCell {
        cell: cell as u16,
        unattainable: counted.unattainable,
        deferrals: counted.deferrals,
        spent_bytes: counted.spent_bytes,
        spent_ops: counted.spent_ops,
        overrun_bytes: counted.overrun_bytes,
        driver_bytes: observed.bytes[IoClass::TierFlush.index()],
        driver_ops: observed.ops[IoClass::TierFlush.index()],
        queued_bytes,
        queued_ops,
        pass_bytes_max: observed.tier_pass_bytes_max,
        pass_ops_max: observed.tier_pass_ops_max,
        groups_deferred: gauge(G::GroupsDeferred),
        group_wait_ns_max: gauge(G::GroupWaitNsMax),
        turn_waits: gauge(G::TurnWaits),
        turn_contended: gauge(G::TurnContended),
        plan_waits: gauge(G::PlanWaits),
        rounds_planned: gauge(G::RoundsPlanned),
        write_retries: gauge(G::WriteRetries),
        flushed: tier_budget_flushed(&node.plane(cell).keyspace(), shape),
    }
}

/// The shape's engagement on one cell: the model present, the tier class
/// offered, every namespace materialized and flushed, and on the retry
/// arm a retry.
fn tier_budget_vacuous(
    scenario: &DurableScenario,
    shape: TierBudgetShape,
    model_absent: u64,
    record: &TierBudgetCell,
    report: &mut DurableReport,
) {
    let (seed, cell) = (scenario.seed, record.cell);
    if (model_absent != 0) != shape.leg.model_absent() {
        report.violations.push(format!(
            "TIER BUDGET VACUOUS seed {seed:#x} cell {cell}: the device model is {}",
            if model_absent != 0 { "absent" } else { "present on the no-model leg" }
        ));
    }
    if record.spent_ops + record.deferrals + record.unattainable == 0 {
        report.violations.push(format!(
            "TIER BUDGET VACUOUS seed {seed:#x} cell {cell}: the tier class never made an \
             offer under the model"
        ));
    }
    if record.flushed.len() != usize::from(shape.namespaces.get()) {
        report.violations.push(format!(
            "TIER BUDGET VACUOUS seed {seed:#x} cell {cell}: {} of {} tier-budget namespaces \
             materialized",
            record.flushed.len(),
            shape.namespaces.get()
        ));
    }
    for &(index, bytes, _, _) in &record.flushed {
        if bytes == 0 {
            report.violations.push(format!(
                "TIER BUDGET VACUOUS seed {seed:#x} cell {cell}: namespace tb{index} never \
                 advanced its flush watermark under the model"
            ));
        }
    }
    if scenario.plant == crate::net::Plant::TierWriteEio && record.write_retries == 0 {
        report.violations.push(format!(
            "TIER-RETRY ARM VACUOUS seed {seed:#x} cell {cell}: the write fault was planted and \
             no tier write was retried"
        ));
    }
}

/// The tier class's oracles on one cell, each independent of the
/// budget's code: no offer above the cap and no overrun, one driver
/// entry's tier bytes within the cap and the pool, every ready group's
/// wait within `n × T_c` (with the leg's engagement), the spend equal to
/// the disk's tier bytes and ops with what a cut caught queued, and the
/// control leg's reads. The no-model leg reads its own clauses in place
/// of the cap's and the wait's ([`tier_no_model_oracles`]).
fn tier_budget_oracles(
    scenario: &DurableScenario,
    leg: TierBudgetLeg,
    bound_ns: u64,
    pass_bound: u64,
    record: &TierBudgetCell,
    report: &mut DurableReport,
) {
    let (seed, cell) = (scenario.seed, record.cell);
    if record.unattainable != 0 || record.overrun_bytes != 0 {
        report.violations.push(format!(
            "TIER-CAP VIOLATION seed {seed:#x} cell {cell}: {} tier offer(s) above the class \
             cap, {} B issued by the overrun (no tier offer is above the cap)",
            record.unattainable, record.overrun_bytes
        ));
    }
    // The round's op bound (ADR-0155 D3: 70 writes and 4 barriers), read
    // from the driver's own per-pass tier count: a wiring read of the op
    // count on every seed. No leg's round comes near the bound (a 1 MiB
    // slice stages a few ops), so the read is vacuous for the bound itself,
    // whose evidence is the round plan's envelope test.
    let round_ops_max = u64::from(
        inf_log::limits::TIER_ROUND_WRITES_MAX + inf_log::limits::TIER_ROUND_BARRIERS_MAX,
    );
    if record.pass_ops_max > round_ops_max {
        report.violations.push(format!(
            "TIER-OPS VIOLATION seed {seed:#x} cell {cell}: one driver entry carried {} tier \
             ops, past a round's {round_ops_max}",
            record.pass_ops_max
        ));
    }
    if leg.model_absent() {
        tier_no_model_oracles(scenario, record, report);
    } else {
        if record.pass_bytes_max > pass_bound {
            report.violations.push(format!(
                "TIER-PASS VIOLATION seed {seed:#x} cell {cell}: one driver entry carried {} B \
                 of tier writes, above the class cap plus the pool, {pass_bound} B from the model",
                record.pass_bytes_max
            ));
        }
        if record.group_wait_ns_max > bound_ns {
            report.violations.push(format!(
                "TIER-TURN VIOLATION seed {seed:#x} cell {cell}: a ready group waited {} ns, \
                 past n × T_c = {bound_ns} ns from the model",
                record.group_wait_ns_max
            ));
        }
    }
    if leg.expects_contention() && (record.turn_waits == 0 || record.turn_contended == 0) {
        report.violations.push(format!(
            "TIER-TURN VACUOUS seed {seed:#x} cell {cell}: the turn refused {} time(s) and was \
             contended {} time(s); the leg needs both",
            record.turn_waits, record.turn_contended
        ));
    }
    let seen_bytes = record.driver_bytes + record.queued_bytes;
    let seen_ops = record.driver_ops + record.queued_ops;
    if record.spent_bytes != seen_bytes || record.spent_ops != seen_ops {
        report.violations.push(format!(
            "TIER-SPEND VIOLATION seed {seed:#x} cell {cell}: the budget spent {} B {} ops, the \
             driver saw {} B {} ops ({} B {} ops queued at the cut)",
            record.spent_bytes,
            record.spent_ops,
            seen_bytes,
            seen_ops,
            record.queued_bytes,
            record.queued_ops
        ));
    }
    // G4's count is judged per iteration by `TierTurnOracle`, its
    // engagement in the recovered life.
    if leg.is_control() && (record.turn_contended != 0 || record.plan_waits != 0) {
        report.violations.push(format!(
            "TIER-CONTROL VIOLATION seed {seed:#x} cell {cell}: one namespace read \
             tier_turn_contended {} tier_plan_waits {}",
            record.turn_contended, record.plan_waits
        ));
    }
}

/// Whether every tier-budget writer has sent its quota and holds no
/// reply outstanding: the no-model leg's fill is done.
fn tier_fill_done(writers: &[Writer]) -> bool {
    writers
        .iter()
        .filter(|w| matches!(w.class, NsClass::TierBudget(_)))
        .all(|w| !w.setup && w.inflight.is_none() && w.sent >= w.quota)
}

/// The fewest tier ops one MAINTAIN pass pushes when it spends the
/// turn's descriptor visits on the no-model leg's round: its writes are
/// full batch windows, one group each, and a group costs its op plus the
/// next op examined and refused, two visits — so a pass of half the
/// visits in ops is a pass that spent them, and a smaller round pushes
/// every op in one pass below that count.
fn tier_visits_spent_ops() -> u64 {
    inf_server::limits::TIER_ISSUE_OPS_PER_TURN as u64 / 2
}

/// The no-model leg's clauses on one cell: with no model every offer is
/// `Now`, so nothing is deferred; and the leg's regime — a round of more
/// ops than one turn's descriptor visits, so the round issues across
/// turns and a retry group is built behind a run of done writes — is
/// read from the driver: one MAINTAIN's entry carried a pass that spent
/// the visits ([`tier_visits_spent_ops`]), else `VACUOUS`.
fn tier_no_model_oracles(
    scenario: &DurableScenario,
    record: &TierBudgetCell,
    report: &mut DurableReport,
) {
    let (seed, cell) = (scenario.seed, record.cell);
    if record.deferrals != 0 {
        report.violations.push(format!(
            "TIER-ABSENT VIOLATION seed {seed:#x} cell {cell}: {} tier offer(s) deferred with no \
             device model (every offer is Now)",
            record.deferrals
        ));
    }
    let spent = tier_visits_spent_ops();
    if record.pass_ops_max < spent {
        report.violations.push(format!(
            "TIER-VISITS VACUOUS seed {seed:#x} cell {cell}: one MAINTAIN pass pushed at most {} \
             tier ops; the leg needs a pass that spent the turn's visits, {spent} ops or more",
            record.pass_ops_max
        ));
    }
}

/// The fair turn's bound on one ready group's wait, `n × T_c` in
/// nanoseconds, from the cell's share and the scenario alone: `T_c = max
/// over the budgeted axes of (B + 3) / r + 2Δ`, `B` one group (the
/// group's byte and op bounds), `r` the tier class's floor share (one
/// twentieth: the eighth floor times the class's four of ten weights),
/// `Δ` the longest interval between two MAINTAIN entries (one scheduler
/// step: its longest advance plus the microsecond every step adds).
fn fair_turn_bound_ns(share: inf_runtime::DeviceModel, namespaces: u64, step_ns_max: u64) -> u64 {
    let delta_ns = step_ns_max + 1_000;
    let axis = |rate_per_s: u64, group: u64| -> Option<u64> {
        let floor = rate_per_s / 20;
        (floor > 0).then(|| (group + 3).saturating_mul(1_000_000_000) / floor + 2 * delta_ns)
    };
    let bytes = axis(share.write_bytes_per_s, inf_log::limits::TIER_GROUP_BYTES_MAX);
    let ops = axis(share.write_ops_per_s, inf_log::limits::TIER_GROUP_OPS_MAX as u64);
    let t_c = bytes.unwrap_or(0).max(ops.unwrap_or(0));
    namespaces * t_c
}

/// The most tier bytes one MAINTAIN pass can draw, from the model: the
/// class cap (the declared slice, or 50 ms of its weighted share) plus the
/// write pool (50 ms of the share) — the budget refills once a pass.
fn tier_pass_bound_bytes(share: inf_runtime::DeviceModel) -> u64 {
    const HORIZON_NS: u64 = 50_000_000;
    let horizon = |rate_per_s: u64| rate_per_s / 1_000 * HORIZON_NS / 1_000_000;
    let cap = inf_log::limits::TIER_GROUP_BYTES_MAX.max(horizon(share.write_bytes_per_s) * 4 / 10);
    cap + horizon(share.write_bytes_per_s)
}

/// The turn's per-iteration oracle, sampled after every scheduler step
/// (one reactor iteration per cell): the plan count rises by at most one
/// per cell per iteration (one plan per cell turn). With the write-fault
/// plant it also arms the sim disk's fault on the first namespace's first
/// tier file on each cell once that file exists.
struct TierTurnOracle {
    rounds_planned: Vec<u64>,
    /// Whether this life arms the write-fault plant (the first life does;
    /// the recovered life's turns are read, never planted).
    arm_faults: bool,
    /// Arm the plant on a cell only once its driver has carried one pass
    /// that spent the turn's descriptor visits (`tier_visits_spent_ops`):
    /// the no-model leg's round of more ops than one turn's visits is
    /// then issuing, so the fault, and the retry it makes, are that
    /// round's.
    arm_after_visits: bool,
    fault_armed: Vec<bool>,
}

impl TierTurnOracle {
    /// Starts from the gauges as they stand (a recovered life's own
    /// boot turns planned before the harness read them), so the first
    /// sample judges one step, never a life's history.
    fn new(node: &Node, cells: u16, arm_faults: bool) -> TierTurnOracle {
        use inf_server::TierFlushGauge as G;
        let rounds_planned = (0..usize::from(cells))
            .map(|cell| node.plane(cell).tier_flush_gauges()[G::RoundsPlanned as usize])
            .collect();
        TierTurnOracle {
            rounds_planned,
            arm_faults,
            arm_after_visits: false,
            fault_armed: vec![false; usize::from(cells)],
        }
    }

    /// Holds the plant's arming until a cell's driver has seen one pass
    /// that spent the turn's visits (the no-model leg).
    fn arm_after_the_visits_bound(&mut self) {
        self.arm_after_visits = true;
    }

    fn sample(
        &mut self,
        node: &Node,
        disk: &SimDisk,
        scenario: &DurableScenario,
        report: &mut DurableReport,
    ) {
        use inf_server::TierFlushGauge as G;
        if scenario.tier_budget.is_none() {
            return;
        }
        for cell in 0..usize::from(scenario.cells) {
            let planned = node.plane(cell).tier_flush_gauges()[G::RoundsPlanned as usize];
            let before = self.rounds_planned[cell];
            if planned > before + 1 {
                report.violations.push(format!(
                    "TIER-PLAN VIOLATION seed {:#x} cell {cell}: {} rounds planned in one \
                     iteration (one plan per cell turn)",
                    scenario.seed,
                    planned - before
                ));
            }
            self.rounds_planned[cell] = planned;
            let visits_seen = !self.arm_after_visits
                || node.cells[cell].0.driver().observed_io().tier_pass_ops_max
                    >= tier_visits_spent_ops();
            if self.arm_faults
                && scenario.plant == crate::net::Plant::TierWriteEio
                && !self.fault_armed[cell]
                && visits_seen
            {
                let keyspace = node.plane(cell).keyspace();
                let first = keyspace
                    .ns_iter()
                    .find(|spec| spec.name.as_slice() == NsClass::TierBudget(0).name())
                    .map(|spec| spec.id);
                if let Some(id) = first {
                    let path = node
                        .data_dir
                        .join(format!("shard-{cell}"))
                        .join(format!("ns-{}", id.0))
                        .join("cold")
                        .join(inf_log::tier_file_name(0));
                    if disk.inject(&path, inf_log::fs::sim::DeviceFault::WriteEio, 1).is_ok() {
                        self.fault_armed[cell] = true;
                    }
                }
            }
        }
    }
}

/// The recovered life's first turns, sampled per step by the one-plan
/// oracle: the first MAINTAIN seals a slice of every namespace whose
/// replayed tail is past its mutable target, so on the eight-namespace
/// leg every namespace is idle with work in that turn and the cell's one
/// plan passes seven over, one planning per turn after it — the regime
/// the leg's engagement requires.
fn tier_recovered_turns(
    node: &mut Node,
    rng: &mut SplitMix64,
    clock: &Rc<VirtualClock>,
    disk: &SimDisk,
    scenario: &DurableScenario,
    report: &mut DurableReport,
) -> std::io::Result<()> {
    use inf_server::TierFlushGauge as G;
    const TURNS: u64 = 64;
    let Some(shape) = scenario.tier_budget else { return Ok(()) };
    let mut oracle = TierTurnOracle::new(node, scenario.cells, false);
    for _ in 0..TURNS {
        report.scheduler_steps += 1;
        node.step(rng, clock, disk, scenario.step_ns_max)?;
        oracle.sample(node, disk, scenario, report);
    }
    if shape.leg.expects_plan_waits() {
        for cell in 0..usize::from(scenario.cells) {
            let gauges = node.plane(cell).tier_flush_gauges();
            if gauges[G::PlanWaits as usize] == 0 {
                report.violations.push(format!(
                    "TIER-PLAN VACUOUS seed {:#x} cell {cell}: no namespace with work was passed \
                     over by the cell's one plan in the recovered life's first {TURNS} turns \
                     (rounds planned {}, rounds {}; (namespace, flushed, ro, tail) {:?})",
                    scenario.seed,
                    gauges[G::RoundsPlanned as usize],
                    gauges[G::Rounds as usize],
                    tier_budget_flushed(&node.plane(cell).keyspace(), shape)
                ));
            }
        }
    }
    Ok(())
}

/// Each tier-budget namespace's flush watermark, ro-boundary and tail
/// past its life origin on one cell, by namespace index, sorted.
fn tier_budget_flushed(
    keyspace: &inf_store::Keyspace,
    shape: TierBudgetShape,
) -> Vec<(u8, u64, u64, u64)> {
    let index_of = |ns: NsId| -> Option<u8> {
        let spec = keyspace.ns_iter().find(|spec| spec.id == ns)?;
        shape.namespaces.indices().find(|&i| NsClass::TierBudget(i).name() == spec.name.as_slice())
    };
    let mut flushed: Vec<(u8, u64, u64, u64)> = keyspace
        .tiered_namespaces()
        .filter_map(|(ns, table)| {
            let index = index_of(ns)?;
            let space = table.space();
            let origin = space.life_origin().to_raw();
            Some((
                index,
                space.flushed().to_raw() - origin,
                space.ro_boundary().to_raw() - origin,
                space.tail().to_raw() - origin,
            ))
        })
        .collect();
    flushed.sort_unstable();
    flushed
}

fn served_preallocs(stats: &inf_server::DurableStats) -> u64 {
    stats.segment_preallocs.saturating_sub(stats.recycle_misses + stats.segment_prealloc_failures)
}

#[derive(Debug, PartialEq, Eq)]
enum OpenFaultVerdict {
    Ok,
    /// No take ever happened: the arm proves nothing on this seed.
    Unreached,
    Violation(String),
}

/// The `recycle_open_fail` arm's rule (ADR-0090 A14, precondition A17):
/// a take is what the point intercepts, so `served` takes and no fire is
/// the vacuity; no take is unreached; a fire with no fresh fallback is
/// the A14 defect.
fn open_fault_verdict(seed: u64, served: u64, fired: u64, fallbacks: u64) -> OpenFaultVerdict {
    if fired == 0 && served > 0 {
        OpenFaultVerdict::Violation(format!(
            "RECYCLE-OPEN FAULT VACUOUS seed {seed:#x}: {served} preallocs found the pool \
             non-empty and the armed point never fired"
        ))
    } else if fired == 0 {
        OpenFaultVerdict::Unreached
    } else if fallbacks == 0 {
        OpenFaultVerdict::Violation(format!(
            "RECYCLE-OPEN FALLBACK MISSING seed {seed:#x}: the point fired {fired}× and no \
             generation fell back fresh"
        ))
    } else {
        OpenFaultVerdict::Ok
    }
}

/// The wedge leg's cut: the first step at which a cell's namespace extents
/// were reclaimed after that cell's reclaim marks first crossed their cap. A
/// merged mark that reclaimed before its later class's barrier would lose the
/// acknowledged blob values those deaths killed to this cut; a correct one
/// reclaims only what is durable, and the audit after the reboot reads
/// every key. Each cell is judged on its own row and its own reclaims: a
/// crossing on one cell says nothing of a reclaim on another.
#[derive(Default)]
struct MarksCut {
    /// Each cell's extents reclaimed as of the last step read.
    reclaimed: Vec<u64>,
}

impl MarksCut {
    fn due(&mut self, node: &Node) -> bool {
        self.reclaimed.resize(node.cells.len(), 0);
        let mut due = false;
        for (cell, last) in self.reclaimed.iter_mut().enumerate() {
            let plane = node.plane(cell);
            let crossed =
                plane.cap_census().row("epoch-marks").is_some_and(|row| row.crossings > 0);
            let reclaimed = plane.keyspace().tiering_extent_stats().reclaimed;
            due |= crossed && reclaimed > *last;
            *last = reclaimed;
        }
        due
    }
}

/// The epoch-marks shape at the cut: each cell's reclaim-marks census row.
/// The wedge leg's engagement (ADR-0163 D2): the row has held its cap and
/// crossed it at least once, else the run met no wedge and is vacuous (token
/// `cap-row-unfilled`). The everysec leg: no crossing, every merge exact
/// (token `everysec-marks-crossed`), with at least two marks held, else
/// vacuous.
fn epoch_marks_scrape(
    scenario: &DurableScenario,
    leg: EpochMarksLeg,
    node: &Node,
    report: &mut DurableReport,
) {
    for cell in 0..scenario.cells {
        let Some(row) = node.plane(usize::from(cell)).cap_census().row("epoch-marks") else {
            let what = format!(
                "VACUOUS seed {:#x} cell {cell}: cap-row-unfilled — no `epoch-marks` row at the \
                 cut",
                scenario.seed
            );
            report.violations.push(what);
            continue;
        };
        report.epoch_marks_cells.push(EpochMarksCell { cell, row });
        let (wedge, held) = (row.high_water, row.entries_max);
        let what = match leg {
            EpochMarksLeg::Wedge if wedge < held || row.crossings == 0 => format!(
                "VACUOUS seed {:#x} cell {cell}: cap-row-unfilled — the `epoch-marks` row held \
                 {wedge} of {held} marks with {} crossing(s): the wedge never filled it",
                scenario.seed, row.crossings
            ),
            EpochMarksLeg::Everysec if row.crossings > 0 => format!(
                "EPOCH MARKS VIOLATION seed {:#x} cell {cell}: everysec-marks-crossed — {} cap \
                 merge(s) with no wedge: a merge that is not exact",
                scenario.seed, row.crossings
            ),
            EpochMarksLeg::Everysec if wedge < 2 => format!(
                "VACUOUS seed {:#x} cell {cell}: everysec-marks-unheld — the `epoch-marks` row \
                 never held two marks",
                scenario.seed
            ),
            EpochMarksLeg::Wedge | EpochMarksLeg::Everysec => continue,
        };
        report.violations.push(what);
    }
}

/// Runs the scenario and seals state evidence after every node has been dropped.
#[must_use]
pub fn run_durable_scenario(scenario: &DurableScenario) -> DurableReport {
    let observer = TraceObserver::default();
    let mut report = run_observed(scenario, observer.clone());
    let sealed = observer.seal();
    report.state_hash = sealed.state_hash;
    report.cap_rows_checked = sealed.census.rows_checked;
    report.cap_rows_crossed = sealed.census.rows_crossed;
    report.violations.extend(sealed.census.violations);
    report
}

#[cfg(test)]
mod tests {
    use super::{OpenFaultVerdict, open_fault_verdict};

    /// F-L19-20: the four corners of the arm's rule. The batch-16 rule
    /// answered the `(served 0, fired 0)` corner with the vacuity
    /// violation — seed `0xd5ee0103`'s shape (6 preallocs, 6 misses).
    #[test]
    fn open_fault_rule_distinguishes_unreached_from_vacuous() {
        assert_eq!(open_fault_verdict(0xd5ee0103, 0, 0, 0), OpenFaultVerdict::Unreached);
        assert!(matches!(
            open_fault_verdict(0xd5ee0003, 3, 0, 0),
            OpenFaultVerdict::Violation(w) if w.contains("VACUOUS") && w.contains("3 preallocs")
        ));
        assert!(matches!(
            open_fault_verdict(0xd5ee0003, 3, 1, 0),
            OpenFaultVerdict::Violation(w) if w.contains("FALLBACK MISSING")
        ));
        assert_eq!(open_fault_verdict(0xd5ee0003, 3, 1, 1), OpenFaultVerdict::Ok);
        // A take in the recovered life alone (served counted there) still
        // needs its fallback counted from that life — the sampling rule.
        assert_eq!(open_fault_verdict(0xd5ee000b, 1, 1, 1), OpenFaultVerdict::Ok);
    }
}
