//! The M2.5-S01 boot-storm scenario: N cells racing recovery across
//! repeated boot cycles — fresh (empty-dir) boots alternating with
//! power-cut reboots of populated dirs — with two oracles aimed at the
//! ADR-0022 D7 wedge class:
//!
//! 1. **The ready path is fsync-free.** [`SimDisk::sync_dir_calls`] must
//!    not move between boot start and `RecoveryBoard::all_ready`: a
//!    blocking metadata sync on a reactor thread is exactly the mechanism
//!    that wedged cell 2 for minutes behind entangled journal writeback.
//!    Reverting the S01 fix (synchronous `create_cell_dirs` /
//!    `create_prealloc` on the boot path) turns this oracle red — the
//!    planted-bug demonstration lives in the test suite.
//! 2. **Barriers still gate durability.** After every boot, an `always`
//!    write must round-trip (its ack is fenced behind the boot barriers
//!    by ledger order), and after a power-cut reboot the previously acked
//!    write must have survived.
//!
//! Cells recover mixed shapes by construction: cycle 0 boots four empty
//! shards; the traffic before each cut populates some cells' logs and
//! leaves others empty (slot routing decides), so reboots race empty and
//! populated recoveries side by side — the S14 fleet inherits this
//! scenario as a permanent oracle.

use std::path::PathBuf;
use std::rc::Rc;

use inf_foundation::hash64;
use inf_foundation::rng::SplitMix64;
use inf_foundation::time::{Nanos, VirtualClock};
use inf_server::SimDisk;

use crate::durable::{DurableScenario, MiniClient, TraceObserver, boot};

/// Scheduler-step budget for a node to reach all-ready: empty and
/// small-log recoveries complete in a handful of steps; the budget only
/// exists so a wedge is a verdict, not a hang.
const READY_STEPS_BUDGET: u64 = 10_000;

#[derive(Clone, Debug)]
pub struct BootStormScenario {
    pub seed: u64,
    pub cells: u16,
    /// Boot cycles. Even cycles boot a fresh data dir (the captured
    /// wedge shape); odd cycles power-cut and reboot the previous dir.
    pub cycles: u32,
    pub step_ns_max: u64,
    /// `always` writes per populate phase (spread across cells by slot).
    pub writes: u32,
}

impl BootStormScenario {
    #[must_use]
    pub fn m2_boot_storm(seed: u64) -> BootStormScenario {
        BootStormScenario { seed, cells: 4, cycles: 6, step_ns_max: 2_000_000, writes: 12 }
    }
}

#[derive(Debug)]
pub struct BootStormReport {
    pub violations: Vec<String>,
    pub boots: u64,
    pub ready_steps_max: u64,
    /// Determinism currency: chained hash over per-cycle
    /// (ready steps, blocking-sync delta) — same seed ⇒ same hash.
    pub trace_hash: u64,
}

impl BootStormReport {
    #[must_use]
    pub fn ok(&self) -> bool {
        self.violations.is_empty()
    }
}

/// Drives one seeded boot storm. See the module docs for the oracles.
#[must_use]
pub fn run_boot_storm_scenario(scenario: &BootStormScenario) -> BootStormReport {
    let clock = Rc::new(VirtualClock::new(Nanos(1)));
    let disk = SimDisk::new();
    let observer = TraceObserver::default();
    let mut rng = SplitMix64::new(scenario.seed ^ 0xB007_5708);
    let mut report =
        BootStormReport { violations: Vec::new(), boots: 0, ready_steps_max: 0, trace_hash: 0 };

    let mut base = DurableScenario::m2_durable(scenario.seed);
    base.cells = scenario.cells;

    let mut dir_epoch = 0u32;
    let mut cycle = 0u32;
    while cycle < scenario.cycles {
        let fresh = cycle.is_multiple_of(2);
        if fresh {
            dir_epoch = cycle;
        }
        let data_dir = PathBuf::from(format!("storm-{dir_epoch}"));

        // ---- boot + the fsync-free-ready oracle window -----------------
        let syncs_before = disk.sync_dir_calls();
        let mut node = match boot(&base, data_dir, &disk, &clock, &observer) {
            Ok(node) => node,
            Err(err) => {
                report.violations.push(format!("cycle {cycle}: boot failed: {err}"));
                return report;
            }
        };
        report.boots += 1;
        let mut ready_steps = 0u64;
        while !node.ready() && ready_steps < READY_STEPS_BUDGET {
            ready_steps += 1;
            if let Err(err) = node.step(&mut rng, &clock, &disk, scenario.step_ns_max) {
                report.violations.push(format!("cycle {cycle}: recovery step failed: {err}"));
                return report;
            }
        }
        if !node.ready() {
            report
                .violations
                .push(format!("cycle {cycle}: not all-ready within {READY_STEPS_BUDGET} steps"));
            return report;
        }
        let sync_delta = disk.sync_dir_calls() - syncs_before;
        if sync_delta != 0 {
            report.violations.push(format!(
                "cycle {cycle}: {sync_delta} blocking sync_dir call(s) on the ready path — \
                 the ADR-0022 D7 wedge mechanism (boot metadata must ride driver barriers)"
            ));
        }
        report.ready_steps_max = report.ready_steps_max.max(ready_steps);
        let mut sample = [0u8; 16];
        sample[..8].copy_from_slice(&ready_steps.to_le_bytes());
        sample[8..].copy_from_slice(&sync_delta.to_le_bytes());
        report.trace_hash = hash64(&sample, report.trace_hash);

        // ---- durability across the boundary ----------------------------
        let mut client = MiniClient::connect(&mut node, 0);
        let ddl: &[&[u8]] = if fresh {
            &[b"INF.NS", b"CREATE", b"alw", b"MODE", b"durable", b"FSYNC", b"always"]
        } else {
            &[b"INF.NS", b"USE", b"alw"]
        };
        match client.call(&mut node, &mut rng, &clock, &disk, scenario.step_ns_max, ddl) {
            Ok(Some(ok)) if ok == b"+OK\r\n" => {}
            other => {
                report.violations.push(format!("cycle {cycle}: ns setup answered {other:?}"));
                return report;
            }
        }
        if fresh {
            let mut use_ok = false;
            match client.call(
                &mut node,
                &mut rng,
                &clock,
                &disk,
                scenario.step_ns_max,
                &[b"INF.NS", b"USE", b"alw"],
            ) {
                Ok(Some(ok)) if ok == b"+OK\r\n" => use_ok = true,
                other => report
                    .violations
                    .push(format!("cycle {cycle}: USE after CREATE answered {other:?}")),
            }
            if !use_ok {
                return report;
            }
            for w in 0..scenario.writes {
                let key = format!("storm:{dir_epoch}:{w}");
                let val = format!("v{w}");
                // The gated ack proves the boot barriers completed: the
                // watermark cannot advance past them (ledger order).
                match client.call(
                    &mut node,
                    &mut rng,
                    &clock,
                    &disk,
                    scenario.step_ns_max,
                    &[b"SET", key.as_bytes(), val.as_bytes()],
                ) {
                    Ok(Some(ok)) if ok == b"+OK\r\n" => {}
                    other => {
                        report.violations.push(format!(
                            "cycle {cycle}: always SET {key} answered {other:?} — a stalled \
                             gated ack means the boot barriers never completed"
                        ));
                        return report;
                    }
                }
            }
        } else {
            // Reboot leg: every write acked before the cut must survive.
            for w in 0..scenario.writes {
                let key = format!("storm:{dir_epoch}:{w}");
                let want = format!("${}\r\nv{w}\r\n", format!("v{w}").len());
                match client.call(
                    &mut node,
                    &mut rng,
                    &clock,
                    &disk,
                    scenario.step_ns_max,
                    &[b"GET", key.as_bytes()],
                ) {
                    Ok(Some(reply)) if reply == want.as_bytes() => {}
                    other => report.violations.push(format!(
                        "cycle {cycle}: acked always write {key} lost across the cut \
                         (got {other:?})"
                    )),
                }
            }
        }

        // ---- cut for the next leg --------------------------------------
        drop(client);
        drop(node);
        if fresh {
            disk.power_cut(scenario.seed ^ u64::from(cycle));
        }
        cycle += 1;
    }
    report
}
