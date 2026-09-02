//! `m2-ns-create-window` — the namespace-creation window (ADR-0103; review
//! of 2026-08-30, C14 / `F-L14-05`): a namespace must not be servable on
//! any cell before its definition is durable in `META`.
//!
//! One seeded run, in phases:
//! 1. boot a two-cell durable node; create a baseline namespace normally
//!    (the control inbox drains every step — the swap lands);
//! 2. **hold the inbox** (the deterministic form of a slow `META`
//!    fdatasync), send `INF.NS CREATE fresh MODE durable FSYNC always`
//!    from one connection, and — over a seeded number of held steps —
//!    have one probe connection per cell attempt `INF.NS USE fresh` at a
//!    seeded step and, if the `USE` is answered `+OK`, an `always` `SET`;
//! 3. cut the power **without draining** — the swap never landed;
//! 4. reboot: the boot must succeed, every cell must report zero
//!    untombstoned-unknown-namespace skips (ADR-0103 D4), and the
//!    pre-fix signature — a probe's `SET` acked in phase 2 while the
//!    namespace is gone after the cut — is a violation. Post-fix the
//!    probes' `USE`s are refused, so nothing was ever promised;
//! 5. the released half: with the inbox draining, create `fresh` again,
//!    write one key per cell, cut, reboot, and find every key.
//!
//! The pre-fix tree answered `+OK` to every probe's `USE` (the DDL
//! applied locally and fanned before it persisted), acked the `SET`s on
//! the log's watermark, and the reboot answered `namespace not found` —
//! the review's exact loss.

use std::path::PathBuf;
use std::rc::Rc;

use inf_foundation::hash64;
use inf_foundation::rng::{Entropy, SplitMix64};
use inf_foundation::time::{Clock, Nanos, VirtualClock};

use crate::durable::{
    DurableScenario, MiniClient, Node, STALL_STEPS, TraceObserver, boot, build_disk,
};

/// The run's verdict and coverage disclosures.
#[derive(Debug)]
pub struct NsCreateWindowReport {
    pub trace: Vec<u8>,
    pub trace_hash: u64,
    pub violations: Vec<String>,
    pub stalled: bool,
    pub commands_done: u64,
    pub scheduler_steps: u64,
    pub sim_seconds: f64,
    /// Steps the swap was held while the probes ran (coverage).
    pub held_steps: u64,
    /// Probe `USE fresh` attempts inside the window, and how many were
    /// refused (post-ADR: all of them).
    pub use_attempts: u64,
    pub use_refused: u64,
    /// `always` writes acked inside the window (post-ADR: zero; pre-fix
    /// every one of them was lost at the cut).
    pub acked_in_window: u64,
    /// Untombstoned-unknown-namespace skips summed over every reboot
    /// and cell (ADR-0103 D4's verifier; zero on every honest boot).
    pub skipped_unknown_ns: u64,
    /// The released half's keys found after its cut (coverage).
    pub released_keys_found: u64,
}

impl NsCreateWindowReport {
    #[must_use]
    pub fn ok(&self) -> bool {
        !self.stalled && self.violations.is_empty()
    }
}

fn fail(report: &mut NsCreateWindowReport, message: String) {
    report.violations.push(message);
}

fn finish(
    mut report: NsCreateWindowReport,
    observer: &TraceObserver,
    clock: &Rc<VirtualClock>,
) -> NsCreateWindowReport {
    report.trace = observer.trace_bytes();
    report.trace_hash = hash64(&report.trace, 0xC14A);
    report.sim_seconds = clock.now().0.saturating_sub(1) as f64 / 1e9;
    report
}

/// Steps the node until every cell is ready (bounded).
fn recover(
    node: &mut Node,
    rng: &mut SplitMix64,
    clock: &Rc<VirtualClock>,
    disk: &inf_server::SimDisk,
    step_ns_max: u64,
    report: &mut NsCreateWindowReport,
) -> Result<(), String> {
    let mut steps = 0u64;
    while !node.ready() {
        steps += 1;
        report.scheduler_steps += 1;
        node.step(rng, clock, disk, step_ns_max).map_err(|e| format!("recovery failed: {e}"))?;
        if steps > STALL_STEPS {
            report.stalled = true;
            return Err("recovery stalled".to_owned());
        }
    }
    Ok(())
}

fn scrape_unknown_skips(node: &Node, cells: u16) -> u64 {
    (0..cells).map(|c| node.control.recovery_board().slot(c).records_skipped_unknown_ns()).sum()
}

/// Runs one seeded `m2-ns-create-window` scenario.
#[must_use]
#[allow(clippy::too_many_lines)] // one linear phase script, like run_durable_scenario
pub fn run_ns_create_window_scenario(seed: u64) -> NsCreateWindowReport {
    let scenario = DurableScenario::m2_durable(seed);
    let clock = Rc::new(VirtualClock::new(Nanos(1)));
    let disk = build_disk(seed, scenario.stall.as_ref());
    let observer = TraceObserver::default();
    let mut rng = SplitMix64::new(seed ^ 0x0C14_C14A);
    let mut report = NsCreateWindowReport {
        trace: Vec::new(),
        trace_hash: 0,
        violations: Vec::new(),
        stalled: false,
        commands_done: 0,
        scheduler_steps: 0,
        sim_seconds: 0.0,
        held_steps: 0,
        use_attempts: 0,
        use_refused: 0,
        acked_in_window: 0,
        skipped_unknown_ns: 0,
        released_keys_found: 0,
    };
    let step_ns_max = scenario.step_ns_max;
    let cells = scenario.cells;

    // ---- phase 1: boot + a baseline namespace -------------------------
    let mut node = match boot(&scenario, PathBuf::from("node"), &disk, &clock, &observer) {
        Ok(node) => node,
        Err(err) => {
            fail(&mut report, format!("boot 1 failed: {err}"));
            return finish(report, &observer, &clock);
        }
    };
    if let Err(err) = recover(&mut node, &mut rng, &clock, &disk, step_ns_max, &mut report) {
        fail(&mut report, format!("boot 1: {err}"));
        return finish(report, &observer, &clock);
    }
    let mut setup = MiniClient::connect(&mut node, 0);
    let create_base: &[&[u8]] =
        &[b"INF.NS", b"CREATE", b"base", b"MODE", b"durable", b"FSYNC", b"always"];
    match setup.call(&mut node, &mut rng, &clock, &disk, step_ns_max, create_base) {
        Ok(Some(ok)) if ok == b"+OK\r\n" => report.commands_done += 1,
        other => {
            fail(&mut report, format!("baseline CREATE answered {other:?}"));
            return finish(report, &observer, &clock);
        }
    }

    // ---- phase 2: the window ------------------------------------------
    node.hold_inbox = true;
    let creator_cell = (rng.next_u64() % u64::from(cells)) as usize;
    let mut creator = MiniClient::connect(&mut node, creator_cell);
    creator.send(
        &mut node,
        &[b"INF.NS", b"CREATE", b"fresh", b"MODE", b"durable", b"FSYNC", b"always"],
    );
    let held_steps = 40 + rng.next_below(400);
    report.held_steps = held_steps;
    // One probe per cell: `USE` at a seeded step inside the window, then
    // a `SET` if the `USE` was answered `+OK`.
    struct Probe {
        client: MiniClient,
        cell: u16,
        use_at: u64,
        sent_use: bool,
        use_reply: Option<Vec<u8>>,
        sent_set: bool,
        set_reply: Option<Vec<u8>>,
        key: Vec<u8>,
    }
    let mut probes: Vec<Probe> = (0..cells)
        .map(|cell| Probe {
            client: MiniClient::connect(&mut node, usize::from(cell)),
            cell,
            use_at: 1 + rng.next_below(held_steps / 2),
            sent_use: false,
            use_reply: None,
            sent_set: false,
            set_reply: None,
            key: format!("c14:cell{cell}").into_bytes(),
        })
        .collect();
    for step in 0..held_steps {
        report.scheduler_steps += 1;
        if let Err(err) = node.step(&mut rng, &clock, &disk, step_ns_max) {
            fail(&mut report, format!("window step {step}: {err}"));
            return finish(report, &observer, &clock);
        }
        if creator.recv(&mut node).is_some() {
            fail(
                &mut report,
                format!("seed {seed:#x}: CREATE fresh was answered while its META swap was held"),
            );
        }
        for probe in &mut probes {
            if !probe.sent_use && step >= probe.use_at {
                probe.client.send(&mut node, &[b"INF.NS", b"USE", b"fresh"]);
                probe.sent_use = true;
                report.use_attempts += 1;
                report.commands_done += 1;
            }
            if probe.sent_use
                && probe.use_reply.is_none()
                && let Some(reply) = probe.client.recv(&mut node)
            {
                if reply.starts_with(b"-ERR") {
                    report.use_refused += 1;
                } else if reply == b"+OK\r\n" {
                    // Pre-fix: servable inside the window.
                    probe.client.send(&mut node, &[b"SET", &probe.key, b"acked-before-durable"]);
                    probe.sent_set = true;
                    report.commands_done += 1;
                } else {
                    fail(&mut report, format!("probe cell {}: USE answered {reply:?}", probe.cell));
                }
                probe.use_reply = Some(reply);
            }
            if probe.sent_set
                && probe.set_reply.is_none()
                && let Some(reply) = probe.client.recv(&mut node)
            {
                if reply == b"+OK\r\n" {
                    report.acked_in_window += 1;
                }
                probe.set_reply = Some(reply);
            }
        }
    }
    // Every probe must have been answered inside the window, or the
    // window proved nothing about it.
    for probe in &probes {
        if probe.sent_use && probe.use_reply.is_none() {
            fail(&mut report, format!("probe cell {}: USE unanswered in the window", probe.cell));
        }
        if probe.sent_set && probe.set_reply.is_none() {
            fail(&mut report, format!("probe cell {}: SET unanswered in the window", probe.cell));
        }
    }
    let acked: Vec<Vec<u8>> = probes
        .iter()
        .filter(|p| p.set_reply.as_deref() == Some(b"+OK\r\n"))
        .map(|p| p.key.clone())
        .collect();

    // ---- phase 3: the cut, swap never drained ---------------------------
    drop(probes);
    drop(creator);
    drop(setup);
    drop(node);
    disk.power_cut(seed ^ 0x0C14_0FF5);

    // ---- phase 4: reboot + the audit ------------------------------------
    let mut node = match boot(&scenario, PathBuf::from("node"), &disk, &clock, &observer) {
        Ok(node) => node,
        Err(err) => {
            fail(&mut report, format!("reboot after the window refused: {err}"));
            return finish(report, &observer, &clock);
        }
    };
    if let Err(err) = recover(&mut node, &mut rng, &clock, &disk, step_ns_max, &mut report) {
        fail(&mut report, format!("reboot after the window: {err}"));
        return finish(report, &observer, &clock);
    }
    let skips = scrape_unknown_skips(&node, cells);
    report.skipped_unknown_ns += skips;
    if skips > 0 {
        fail(
            &mut report,
            format!(
                "seed {seed:#x}: {skips} tail record(s) skipped for a namespace no tombstone \
                 explains (ADR-0103 D4)"
            ),
        );
    }
    let mut audit = MiniClient::connect(&mut node, 0);
    let use_fresh: &[&[u8]] = &[b"INF.NS", b"USE", b"fresh"];
    let served = match audit.call(&mut node, &mut rng, &clock, &disk, step_ns_max, use_fresh) {
        Ok(Some(reply)) => reply == b"+OK\r\n",
        other => {
            fail(&mut report, format!("post-cut USE fresh answered {other:?}"));
            return finish(report, &observer, &clock);
        }
    };
    if !acked.is_empty() {
        // The review's signature: writes acked, definition gone.
        if !served {
            fail(
                &mut report,
                format!(
                    "seed {seed:#x}: {} `always` write(s) acked into 'fresh' before its \
                     definition was durable, and the namespace is gone after the cut (C14)",
                    acked.len()
                ),
            );
        } else {
            for key in &acked {
                let get: &[&[u8]] = &[b"GET", key];
                match audit.call(&mut node, &mut rng, &clock, &disk, step_ns_max, get) {
                    Ok(Some(reply)) if reply.starts_with(b"$20\r\n") => {}
                    other => fail(
                        &mut report,
                        format!("seed {seed:#x}: acked key {key:?} after the cut: {other:?}"),
                    ),
                }
            }
        }
        fail(
            &mut report,
            format!(
                "seed {seed:#x}: {} write(s) were served before the namespace's definition was \
                 durable (ADR-0103 D1)",
                acked.len()
            ),
        );
    }

    // ---- phase 5: the released half ------------------------------------
    // (`fresh` may or may not exist now — nothing was promised; use a
    // fresh name so the phase is the same on both arms.)
    let create2: &[&[u8]] =
        &[b"INF.NS", b"CREATE", b"fresh2", b"MODE", b"durable", b"FSYNC", b"always"];
    match audit.call(&mut node, &mut rng, &clock, &disk, step_ns_max, create2) {
        Ok(Some(ok)) if ok == b"+OK\r\n" => report.commands_done += 1,
        other => {
            fail(&mut report, format!("released CREATE answered {other:?}"));
            return finish(report, &observer, &clock);
        }
    }
    let mut writers: Vec<(MiniClient, Vec<u8>)> = (0..cells)
        .map(|cell| {
            (
                MiniClient::connect(&mut node, usize::from(cell)),
                format!("rel:cell{cell}").into_bytes(),
            )
        })
        .collect();
    for (client, key) in &mut writers {
        let use2: &[&[u8]] = &[b"INF.NS", b"USE", b"fresh2"];
        match client.call(&mut node, &mut rng, &clock, &disk, step_ns_max, use2) {
            Ok(Some(ok)) if ok == b"+OK\r\n" => {}
            other => {
                fail(&mut report, format!("released USE answered {other:?}"));
                return finish(report, &observer, &clock);
            }
        }
        let set: &[&[u8]] = &[b"SET", key, b"durable-definition"];
        match client.call(&mut node, &mut rng, &clock, &disk, step_ns_max, set) {
            Ok(Some(ok)) if ok == b"+OK\r\n" => report.commands_done += 2,
            other => {
                fail(&mut report, format!("released SET answered {other:?}"));
                return finish(report, &observer, &clock);
            }
        }
    }
    let keys: Vec<Vec<u8>> = writers.iter().map(|(_, k)| k.clone()).collect();
    drop(writers);
    drop(audit);
    drop(node);
    disk.power_cut(seed ^ 0x0C14_0FF6);
    let mut node = match boot(&scenario, PathBuf::from("node"), &disk, &clock, &observer) {
        Ok(node) => node,
        Err(err) => {
            fail(&mut report, format!("reboot after the released half refused: {err}"));
            return finish(report, &observer, &clock);
        }
    };
    if let Err(err) = recover(&mut node, &mut rng, &clock, &disk, step_ns_max, &mut report) {
        fail(&mut report, format!("reboot after the released half: {err}"));
        return finish(report, &observer, &clock);
    }
    let skips = scrape_unknown_skips(&node, cells);
    report.skipped_unknown_ns += skips;
    if skips > 0 {
        fail(&mut report, format!("seed {seed:#x}: {skips} untombstoned unknown skips (life 3)"));
    }
    let mut reader = MiniClient::connect(&mut node, 0);
    let use2: &[&[u8]] = &[b"INF.NS", b"USE", b"fresh2"];
    match reader.call(&mut node, &mut rng, &clock, &disk, step_ns_max, use2) {
        Ok(Some(ok)) if ok == b"+OK\r\n" => {}
        other => {
            fail(&mut report, format!("USE fresh2 after the cut answered {other:?}"));
            return finish(report, &observer, &clock);
        }
    }
    for key in &keys {
        let get: &[&[u8]] = &[b"GET", key];
        match reader.call(&mut node, &mut rng, &clock, &disk, step_ns_max, get) {
            Ok(Some(reply)) if reply == b"$18\r\ndurable-definition\r\n" => {
                report.released_keys_found += 1;
            }
            other => fail(
                &mut report,
                format!("seed {seed:#x}: acked key {key:?} after the released cut: {other:?}"),
            ),
        }
    }
    drop(reader);
    drop(node);
    finish(report, &observer, &clock)
}
