//! `m2-ns-ddl-race` — concurrent namespace DDL (ADR-0108; review of
//! 2026-08-30, the batch-8 residual ADR-0103 recorded and did not fix):
//! a `CREATE` and a `DROP` of one name from two cells whose fan legs
//! cross, and a `CREATE` whose fan is refused on one peer.
//!
//! One seeded run on a four-cell durable node, in phases:
//! 1. boot;
//! 2. **the crossing**: `INF.NS CREATE x` from a seeded cell; after a
//!    seeded number of steps, `INF.NS DROP x` from a different cell; step
//!    until both are answered. The pre-fix tree fanned each program's
//!    legs one peer at a time with nothing serializing the two programs,
//!    so the `DROP` could land between two `CREATE` legs: the cells the
//!    `CREATE` had reached dropped `x`, the cells it had not reached
//!    created it afterwards, and `META` (persisted by the `DROP`) named
//!    nothing — a **phantom namespace** servable on some cells only, with
//!    `+OK` to both clients. The audit asks every cell `INF.NS USE x`: the
//!    served set must be empty or complete, and must agree with the two
//!    replies (`DROP +OK` ⇒ nobody serves; `CREATE +OK` and `DROP` "not
//!    found" ⇒ everybody serves);
//! 3. a power cut and a reboot: every cell now seeds from `META`, so the
//!    served set after the cut must equal the served set before it —
//!    a cell that served `x` while `META` did not name it is the phantom;
//!    zero untombstoned-unknown-namespace skips (ADR-0103 D4);
//! 4. **the refused leg**: with `ns_create_fan_refused` armed on a seeded
//!    peer leg, `INF.NS CREATE y` must answer an error and leave `y` on
//!    no cell — before and after another cut. Pre-fix the origin and the
//!    peers that accepted their leg kept serving `y` (the partial-fan
//!    residual);
//! 5. the released half: a plain `CREATE z`, one key per cell, a cut, and
//!    every key found — DDL still works after the races.

use std::path::PathBuf;
use std::rc::Rc;

use inf_foundation::fault::FaultSpec;
use inf_foundation::hash64;
use inf_foundation::rng::{Entropy, SplitMix64};
use inf_foundation::time::{Clock, Nanos, VirtualClock};

use crate::durable::{
    DurableScenario, MiniClient, Node, STALL_STEPS, TraceObserver, boot, build_disk,
};

const NS_CREATE_FAN_REFUSED: &str = "ns_create_fan_refused";
/// Bound on steps a DDL race may take to answer both clients.
const DDL_STEPS: u64 = 20_000;

/// The run's verdict and coverage disclosures.
#[derive(Debug)]
pub struct NsDdlRaceReport {
    pub trace: Vec<u8>,
    pub trace_hash: u64,
    pub violations: Vec<String>,
    pub stalled: bool,
    pub commands_done: u64,
    pub scheduler_steps: u64,
    pub sim_seconds: f64,
    /// Steps between the `CREATE` and the `DROP` (coverage).
    pub drop_delay: u64,
    /// Steps the origin was frozen while a `DROP` went out (summed over
    /// the rounds; 0 = every `DROP` followed an answered `CREATE`).
    pub freeze_steps: u64,
    /// Rounds whose `DROP` went out while the origin was frozen mid-fan.
    pub crossings: u64,
    /// `DROP` answered `+OK` (it found the namespace — the crossing
    /// exercised the serialized order).
    pub drop_found: bool,
    /// Cells serving `x` after the race, before the cut.
    pub served_before_cut: u16,
    /// Cells serving `x` after the reboot.
    pub served_after_cut: u16,
    /// The refused leg's ordinal (1-based over the peers) — coverage.
    pub refused_leg: u64,
    /// Cells serving `y` after the refused-leg `CREATE` (post-fix: 0).
    pub partial_served: u16,
    /// Untombstoned-unknown-namespace skips over every reboot.
    pub skipped_unknown_ns: u64,
    /// Keys found after the released half's cut.
    pub released_keys_found: u64,
}

impl NsDdlRaceReport {
    #[must_use]
    pub fn ok(&self) -> bool {
        !self.stalled && self.violations.is_empty()
    }
}

fn fail(report: &mut NsDdlRaceReport, message: String) {
    report.violations.push(message);
}

fn finish(
    mut report: NsDdlRaceReport,
    observer: &TraceObserver,
    clock: &Rc<VirtualClock>,
) -> NsDdlRaceReport {
    report.trace = observer.trace_bytes();
    report.trace_hash = hash64(&report.trace, 0x0DD1);
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
    report: &mut NsDdlRaceReport,
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

/// `-ERR namespace 'x' not found` (the admin surface's spelling).
fn is_not_found(reply: &[u8]) -> bool {
    reply.starts_with(b"-ERR namespace") && reply.windows(9).any(|w| w == b"not found")
}

/// Which cells answer `+OK` to `INF.NS USE name` (one fresh client each).
#[allow(clippy::too_many_arguments)]
fn served_on(
    node: &mut Node,
    rng: &mut SplitMix64,
    clock: &Rc<VirtualClock>,
    disk: &inf_server::SimDisk,
    step_ns_max: u64,
    cells: u16,
    name: &[u8],
    report: &mut NsDdlRaceReport,
) -> Result<Vec<u16>, String> {
    let mut served = Vec::new();
    for cell in 0..cells {
        let mut probe = MiniClient::connect(node, usize::from(cell));
        let use_ns: &[&[u8]] = &[b"INF.NS", b"USE", name];
        match probe.call(node, rng, clock, disk, step_ns_max, use_ns) {
            Ok(Some(reply)) if reply == b"+OK\r\n" => served.push(cell),
            Ok(Some(reply)) if is_not_found(&reply) => {}
            Ok(Some(reply)) => {
                return Err(format!(
                    "cell {cell}: USE answered {:?}",
                    String::from_utf8_lossy(&reply)
                ));
            }
            Ok(None) => {
                report.stalled = true;
                return Err(format!("cell {cell}: USE stalled"));
            }
            Err(e) => return Err(format!("cell {cell}: USE failed: {e}")),
        }
        report.commands_done += 1;
    }
    Ok(served)
}

/// Runs one seeded `m2-ns-ddl-race` scenario.
#[must_use]
#[allow(clippy::too_many_lines)] // one linear phase script, like run_ns_create_window_scenario
pub fn run_ns_ddl_race_scenario(seed: u64) -> NsDdlRaceReport {
    let scenario = DurableScenario { cells: 4, ..DurableScenario::m2_durable(seed) };
    let clock = Rc::new(VirtualClock::new(Nanos(1)));
    let disk = build_disk(seed, scenario.stall.as_ref());
    let observer = TraceObserver::default();
    let mut rng = SplitMix64::new(seed ^ 0x0DD1_0DD1);
    let mut report = NsDdlRaceReport {
        trace: Vec::new(),
        trace_hash: 0,
        violations: Vec::new(),
        stalled: false,
        commands_done: 0,
        scheduler_steps: 0,
        sim_seconds: 0.0,
        drop_delay: 0,
        freeze_steps: 0,
        crossings: 0,
        drop_found: false,
        served_before_cut: 0,
        served_after_cut: 0,
        refused_leg: 0,
        partial_served: 0,
        skipped_unknown_ns: 0,
        released_keys_found: 0,
    };
    let step_ns_max = scenario.step_ns_max;
    let cells = scenario.cells;
    // Alternate the namespace mode by seed: durable exercises the
    // tombstone path of a crossed DROP, memory the pending-create path.
    let mode: &[u8] = if seed.is_multiple_of(2) { b"durable" } else { b"memory" };
    let create = |name: &'static [u8]| -> Vec<Vec<u8>> {
        let mut argv = vec![
            b"INF.NS".to_vec(),
            b"CREATE".to_vec(),
            name.to_vec(),
            b"MODE".to_vec(),
            mode.to_vec(),
        ];
        if mode == b"durable" {
            argv.push(b"FSYNC".to_vec());
            argv.push(b"always".to_vec());
        }
        argv
    };

    // ---- phase 1: boot ---------------------------------------------------
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

    // ---- phase 2: the crossings ------------------------------------------
    // Two rounds per seed, one name each. The origin is frozen for a
    // seeded window starting at a seeded step after its `CREATE` went
    // out (a park, an I/O stall): its sequential fan pauses at whatever
    // leg it had reached. A watcher on the dropper's cell asks `USE`
    // every step and sends the `DROP` the moment its own leg is visible
    // — while the origin is still frozen whenever the freeze caught the
    // fan mid-way, which is the crossing a same-pace race never has.
    let mut rounds: Vec<(&'static [u8], Vec<u16>)> = Vec::new();
    for (round, name) in [(0u8, &b"x0"[..]), (1u8, &b"x1"[..])] {
        let creator_cell = (rng.next_u64() % u64::from(cells)) as usize;
        let dropper_cell = (creator_cell + 1 + (rng.next_u64() % u64::from(cells - 1)) as usize)
            % usize::from(cells);
        let freeze_at = 2 + rng.next_below(28);
        let freeze_len = 12 + rng.next_below(28);
        let mut creator = MiniClient::connect(&mut node, creator_cell);
        let mut dropper = MiniClient::connect(&mut node, dropper_cell);
        let mut watcher = MiniClient::connect(&mut node, dropper_cell);
        let create_x = create(name);
        let create_x_argv: Vec<&[u8]> = create_x.iter().map(Vec::as_slice).collect();
        creator.send(&mut node, &create_x_argv);
        report.commands_done += 1;
        let (mut create_reply, mut drop_reply): (Option<Vec<u8>>, Option<Vec<u8>>) = (None, None);
        let mut sent_drop = false;
        let mut probe_outstanding = false;
        let mut drop_delay = 0u64;
        let mut frozen_at_drop = false;
        for step in 0..DDL_STEPS {
            report.scheduler_steps += 1;
            if step == freeze_at && create_reply.is_none() {
                node.frozen = Some((creator_cell, freeze_len));
            }
            if let Err(err) = node.step(&mut rng, &clock, &disk, step_ns_max) {
                fail(&mut report, format!("round {round} step {step}: {err}"));
                return finish(report, &observer, &clock);
            }
            if create_reply.is_none() {
                create_reply = creator.recv(&mut node);
            }
            if !sent_drop {
                if probe_outstanding {
                    if let Some(reply) = watcher.recv(&mut node) {
                        probe_outstanding = false;
                        if reply == b"+OK\r\n" || create_reply.is_some() {
                            frozen_at_drop = node.frozen.is_some_and(|(_, left)| left > 0)
                                && create_reply.is_none();
                            dropper.send(&mut node, &[b"INF.NS", b"DROP", name]);
                            sent_drop = true;
                            drop_delay = step;
                            report.commands_done += 1;
                        }
                    }
                } else {
                    watcher.send(&mut node, &[b"INF.NS", b"USE", name]);
                    probe_outstanding = true;
                }
            }
            if sent_drop && drop_reply.is_none() {
                drop_reply = dropper.recv(&mut node);
            }
            if create_reply.is_some() && drop_reply.is_some() {
                break;
            }
        }
        node.frozen = None;
        if round == 0 {
            report.drop_delay = drop_delay;
        }
        report.freeze_steps += u64::from(frozen_at_drop) * freeze_len;
        report.crossings += u64::from(frozen_at_drop);
        drop(watcher);
        let (Some(create_reply), Some(drop_reply)) = (create_reply, drop_reply) else {
            report.stalled = true;
            fail(&mut report, format!("seed {seed:#x}: round {round} never answered both clients"));
            return finish(report, &observer, &clock);
        };
        let drop_found = drop_reply == b"+OK\r\n";
        report.drop_found |= drop_found;
        if create_reply != b"+OK\r\n" {
            fail(
                &mut report,
                format!(
                    "seed {seed:#x}: CREATE {} answered {:?}",
                    String::from_utf8_lossy(name),
                    String::from_utf8_lossy(&create_reply)
                ),
            );
        }
        if !drop_found && !is_not_found(&drop_reply) {
            fail(
                &mut report,
                format!(
                    "seed {seed:#x}: DROP {} answered {:?}",
                    String::from_utf8_lossy(name),
                    String::from_utf8_lossy(&drop_reply)
                ),
            );
        }
        let served = match served_on(
            &mut node,
            &mut rng,
            &clock,
            &disk,
            step_ns_max,
            cells,
            name,
            &mut report,
        ) {
            Ok(served) => served,
            Err(err) => {
                fail(&mut report, format!("seed {seed:#x}: round {round} audit: {err}"));
                return finish(report, &observer, &clock);
            }
        };
        if !served.is_empty() && served.len() != usize::from(cells) {
            fail(
                &mut report,
                format!(
                    "seed {seed:#x}: phantom namespace — '{}' is served on cells {served:?} of \
                     {cells} after CREATE {:?} / DROP {:?} (origin {creator_cell} frozen \
                     {freeze_len} steps from step {freeze_at}, DROP from {dropper_cell} at step \
                     {drop_delay})",
                    String::from_utf8_lossy(name),
                    String::from_utf8_lossy(&create_reply),
                    String::from_utf8_lossy(&drop_reply)
                ),
            );
        }
        if drop_found && !served.is_empty() {
            fail(
                &mut report,
                format!(
                    "seed {seed:#x}: DROP {} answered +OK but cells {served:?} still serve it",
                    String::from_utf8_lossy(name)
                ),
            );
        }
        if !drop_found && create_reply == b"+OK\r\n" && served.len() != usize::from(cells) {
            fail(
                &mut report,
                format!(
                    "seed {seed:#x}: CREATE {} answered +OK, DROP found nothing, yet only cells \
                     {served:?} serve it",
                    String::from_utf8_lossy(name)
                ),
            );
        }
        if round == 0 {
            report.served_before_cut = served.len() as u16;
        }
        rounds.push((name, served));
        drop(creator);
        drop(dropper);
    }

    // ---- phase 3: the cut + reboot audit --------------------------------
    drop(node);
    disk.power_cut(seed ^ 0x0DD1_0FF5);
    let mut node = match boot(&scenario, PathBuf::from("node"), &disk, &clock, &observer) {
        Ok(node) => node,
        Err(err) => {
            fail(&mut report, format!("reboot after the races refused: {err}"));
            return finish(report, &observer, &clock);
        }
    };
    if let Err(err) = recover(&mut node, &mut rng, &clock, &disk, step_ns_max, &mut report) {
        fail(&mut report, format!("reboot after the races: {err}"));
        return finish(report, &observer, &clock);
    }
    let skips = scrape_unknown_skips(&node, cells);
    report.skipped_unknown_ns += skips;
    if skips > 0 {
        fail(
            &mut report,
            format!("seed {seed:#x}: {skips} untombstoned unknown-ns skips (life 2)"),
        );
    }
    for (round, (name, served)) in rounds.iter().enumerate() {
        let served_after = match served_on(
            &mut node,
            &mut rng,
            &clock,
            &disk,
            step_ns_max,
            cells,
            name,
            &mut report,
        ) {
            Ok(served) => served,
            Err(err) => {
                fail(&mut report, format!("seed {seed:#x}: post-cut audit: {err}"));
                return finish(report, &observer, &clock);
            }
        };
        if round == 0 {
            report.served_after_cut = served_after.len() as u16;
        }
        if served_after.len() != served.len() {
            fail(
                &mut report,
                format!(
                    "seed {seed:#x}: META disagrees with the cells — '{}' served on {served:?} \
                     before the cut, on {served_after:?} after it",
                    String::from_utf8_lossy(name)
                ),
            );
        }
    }

    // ---- phase 4: the refused leg ---------------------------------------
    let refused_leg = 1 + rng.next_below(u64::from(cells) - 1);
    report.refused_leg = refused_leg;
    inf_foundation::fault::arm(NS_CREATE_FAN_REFUSED, FaultSpec::Nth(refused_leg));
    let creator_cell = (rng.next_u64() % u64::from(cells)) as usize;
    let mut creator = MiniClient::connect(&mut node, creator_cell);
    let create_y = create(b"y");
    let create_y_argv: Vec<&[u8]> = create_y.iter().map(Vec::as_slice).collect();
    match creator.call(&mut node, &mut rng, &clock, &disk, step_ns_max, &create_y_argv) {
        Ok(Some(reply)) if reply.starts_with(b"-") => report.commands_done += 1,
        Ok(Some(reply)) => fail(
            &mut report,
            format!("seed {seed:#x}: CREATE y with a refused leg answered {reply:?}"),
        ),
        Ok(None) => {
            report.stalled = true;
            fail(&mut report, format!("seed {seed:#x}: CREATE y stalled"));
            return finish(report, &observer, &clock);
        }
        Err(e) => {
            fail(&mut report, format!("seed {seed:#x}: CREATE y failed: {e}"));
            return finish(report, &observer, &clock);
        }
    }
    inf_foundation::fault::disarm(NS_CREATE_FAN_REFUSED);
    drop(creator);
    let partial = match served_on(
        &mut node,
        &mut rng,
        &clock,
        &disk,
        step_ns_max,
        cells,
        b"y",
        &mut report,
    ) {
        Ok(served) => served,
        Err(err) => {
            fail(&mut report, format!("seed {seed:#x}: refused-leg audit: {err}"));
            return finish(report, &observer, &clock);
        }
    };
    report.partial_served = partial.len() as u16;
    if !partial.is_empty() {
        fail(
            &mut report,
            format!(
                "seed {seed:#x}: partial fan — CREATE y answered an error (leg {refused_leg} \
                 refused) but cells {partial:?} serve it"
            ),
        );
    }
    drop(node);
    disk.power_cut(seed ^ 0x0DD1_0FF6);
    let mut node = match boot(&scenario, PathBuf::from("node"), &disk, &clock, &observer) {
        Ok(node) => node,
        Err(err) => {
            fail(&mut report, format!("reboot after the refused leg refused: {err}"));
            return finish(report, &observer, &clock);
        }
    };
    if let Err(err) = recover(&mut node, &mut rng, &clock, &disk, step_ns_max, &mut report) {
        fail(&mut report, format!("reboot after the refused leg: {err}"));
        return finish(report, &observer, &clock);
    }
    let skips = scrape_unknown_skips(&node, cells);
    report.skipped_unknown_ns += skips;
    if skips > 0 {
        fail(
            &mut report,
            format!("seed {seed:#x}: {skips} untombstoned unknown-ns skips (life 3)"),
        );
    }
    match served_on(&mut node, &mut rng, &clock, &disk, step_ns_max, cells, b"y", &mut report) {
        Ok(served) if served.is_empty() => {}
        Ok(served) => fail(
            &mut report,
            format!("seed {seed:#x}: META names 'y' after a refused CREATE — served on {served:?}"),
        ),
        Err(err) => {
            fail(&mut report, format!("seed {seed:#x}: post-cut refused-leg audit: {err}"));
            return finish(report, &observer, &clock);
        }
    }

    // ---- phase 5: the released half -------------------------------------
    let mut admin = MiniClient::connect(&mut node, 0);
    let create_z = create(b"z");
    let create_z_argv: Vec<&[u8]> = create_z.iter().map(Vec::as_slice).collect();
    match admin.call(&mut node, &mut rng, &clock, &disk, step_ns_max, &create_z_argv) {
        Ok(Some(ok)) if ok == b"+OK\r\n" => report.commands_done += 1,
        other => {
            fail(&mut report, format!("released CREATE z answered {other:?}"));
            return finish(report, &observer, &clock);
        }
    }
    let mut writers: Vec<(MiniClient, Vec<u8>)> = (0..cells)
        .map(|cell| {
            (
                MiniClient::connect(&mut node, usize::from(cell)),
                format!("ddl:cell{cell}").into_bytes(),
            )
        })
        .collect();
    for (client, key) in &mut writers {
        let use_z: &[&[u8]] = &[b"INF.NS", b"USE", b"z"];
        match client.call(&mut node, &mut rng, &clock, &disk, step_ns_max, use_z) {
            Ok(Some(ok)) if ok == b"+OK\r\n" => {}
            other => {
                fail(&mut report, format!("released USE z answered {other:?}"));
                return finish(report, &observer, &clock);
            }
        }
        let set: &[&[u8]] = &[b"SET", key, b"after-the-races"];
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
    drop(admin);
    drop(node);
    if mode == b"memory" {
        // A memory namespace's keys do not survive a cut; the released
        // half proves the definition does (USE succeeds) — the keys are
        // the durable seeds' evidence.
        report.released_keys_found = keys.len() as u64;
        return finish(report, &observer, &clock);
    }
    disk.power_cut(seed ^ 0x0DD1_0FF7);
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
        fail(&mut report, format!("seed {seed:#x}: {skips} untombstoned unknown skips (life 4)"));
    }
    let mut reader = MiniClient::connect(&mut node, 0);
    let use_z: &[&[u8]] = &[b"INF.NS", b"USE", b"z"];
    match reader.call(&mut node, &mut rng, &clock, &disk, step_ns_max, use_z) {
        Ok(Some(ok)) if ok == b"+OK\r\n" => {}
        other => {
            fail(&mut report, format!("USE z after the cut answered {other:?}"));
            return finish(report, &observer, &clock);
        }
    }
    for key in &keys {
        let get: &[&[u8]] = &[b"GET", key];
        match reader.call(&mut node, &mut rng, &clock, &disk, step_ns_max, get) {
            Ok(Some(reply)) if reply == b"$15\r\nafter-the-races\r\n" => {
                report.released_keys_found += 1;
            }
            other => fail(
                &mut report,
                format!("seed {seed:#x}: key {key:?} after the released cut: {other:?}"),
            ),
        }
    }
    drop(reader);
    drop(node);
    finish(report, &observer, &clock)
}
