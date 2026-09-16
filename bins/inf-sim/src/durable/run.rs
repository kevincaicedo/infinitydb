//! The durable scenario's phase script (`run_durable_scenario`): seeded
//! traffic, cuts, reboots, and the per-phase oracles it drives.

use super::*;

// ---- the run ---------------------------------------------------------------

/// Runs one seeded durable scenario: boot → DDL → seeded traffic → power
/// cut mid-run → reboot (optionally cut again mid-recovery) → recover →
/// audit every ledger key against the §8.2 admissible-state rule.
#[allow(clippy::too_many_lines, reason = "one linear phase script, like run_scenario")]
#[must_use]
pub fn run_durable_scenario(scenario: &DurableScenario) -> DurableReport {
    let clock = Rc::new(VirtualClock::new(Nanos(1)));
    let disk = build_disk(scenario.seed, scenario.stall.as_ref());
    if let Some(allowed) = scenario.ckpt_direct_refused_after {
        disk.refuse_direct_writes_after(allowed);
    }
    let observer = TraceObserver::default();
    let mut rng = SplitMix64::new(scenario.seed ^ 0xD07A_B1E5);
    let mut report = DurableReport {
        trace: Vec::new(),
        trace_hash: 0,
        violations: Vec::new(),
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
        frame_waits_pace: 0,
        write_stall_max_us: 0,
        reopened_packed_tails: 0,
        segments_recycled: 0,
        recycle_misses: 0,
        recycle_fallbacks: 0,
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
        disk.power_cut(scenario.seed ^ 0x0FF5_EED2);
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
    for step in 0..cut_step {
        report.scheduler_steps += 1;
        if let Err(err) = node.step(&mut rng, &clock, &disk, scenario.step_ns_max) {
            fail(&mut report, format!("traffic phase: {err}"));
            return finish(report, &observer, &clock);
        }
        log_oracle.sample(&node, &mut report);
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
                _ => 0,
            };
            if think > 0 {
                writer.idle_until = clock.now() + Nanos(writer.rng.next_below(think));
            }
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
    for cell in 0..usize::from(scenario.cells) {
        if let Some(stats) = node.plane(cell).durable_stats() {
            report.frames_in_flight_max =
                report.frames_in_flight_max.max(stats.frames_in_flight_max);
            report.frame_waits_barrier += stats.frame_waits_barrier;
            report.frame_waits_rotation += stats.frame_waits_rotation;
            report.frame_waits_reorder += stats.frame_waits_reorder;
            report.frame_waits_fill += stats.frame_waits_fill;
            report.frame_waits_group += stats.frame_waits_group;
            report.frame_waits_pace += stats.frame_waits_pace;
            report.write_stall_max_us = report.write_stall_max_us.max(stats.write_stall_max_us);
            report.segments_recycled += stats.segments_recycled;
            report.recycle_misses += stats.recycle_misses;
            report.recycle_fallbacks += stats.recycle_fallbacks;
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
                let served = stats
                    .segment_preallocs
                    .saturating_sub(stats.recycle_misses + stats.segment_prealloc_failures);
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
            }
        }
    }
    engagement_checks(scenario, &node, &mut report);
    if scenario.budget_oracle {
        budget_oracles(scenario, &node, clock.now(), &mut report);
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
                "REORDER WINDOW NOT ENGAGED seed {:#x}: the wedged device never filled the \
                 completion ledger's window (frames_in_flight_max {depth}) — the scenario \
                 proves nothing about the bound",
                scenario.seed
            ),
        );
    }
    drop(node); // the process dies: in-flight state vanishes
    disk.power_cut(scenario.seed ^ 0x0FF5_EED0);
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
        disk.power_cut(scenario.seed ^ 0x0FF5_EED1 ^ boots);
    };
    let mut node = node;
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
                _ => {}
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
        if fired == 0 {
            fail(
                &mut report,
                format!(
                    "RECYCLE-OPEN FAULT VACUOUS seed {:#x}: no pooled file was reused",
                    scenario.seed
                ),
            );
        } else if report.recycle_fallbacks == 0 {
            fail(
                &mut report,
                format!(
                    "RECYCLE-OPEN FALLBACK MISSING seed {:#x}: the point fired {fired}× and no \
                     generation fell back fresh",
                    scenario.seed
                ),
            );
        }
    }

    finish(report, &observer, &clock)
}
