//! `m0-capacity`'s small-credit leg: the work one cell parks for a peer
//! (ADR-0177 D6), on a mesh of 8 data credits a peer. 8 clients of X each
//! send one `SET` of a 4,096 B value, to a key Y owns, in a tiered `always`
//! namespace, in one step, so every apply reaches Y's fabric drain as a
//! tiered apply that Y diverts to the apply pump it makes on demand for X,
//! and every verdict waits in Y's gated deque for X until Y's watermark
//! covers it. The oracles read the wire alone: every `SET` answers `+OK`,
//! and Y's `INFO tripwires` reads its parked bytes'
//! high water above 0 (else `VACUOUS`: nothing was parked), its parked bytes
//! back at 0 once every reply is in (the gauge the pops move), and no op
//! refused at a full deque on any cell (derived never). The census reads
//! every row's high water within its cap when the node drops.

use std::path::PathBuf;
use std::rc::Rc;

use inf_foundation::hash64;
use inf_foundation::rng::SplitMix64;
use inf_foundation::time::{Nanos, VirtualClock};

use super::{CELLS, CapacityReport, CloseLeg, X, Y, names_on};
use crate::driver::Violations;
use crate::durable::{
    DurableScenario, MiniClient, Node, STALL_STEPS, TraceObserver, boot_meshed, build_disk,
};

/// A mesh of 8 data credits a peer (the leg's `d`), on rings with the
/// reply headroom the plane's mesh check reads.
const SMALL_CREDIT_MESH: inf_fabric::MeshConfig = crate::harness::scenario_mesh(1024, 8);
/// The tiered namespace the leg writes.
const NS_NAME: &[u8] = b"sc";
/// The `SET`s X pipelines toward Y, and each value's bytes.
const SETS: usize = 8;
const VALUE_BYTES: usize = 4_096;
/// Steps the node takes once every reply is in, so the pumps and the
/// `gated` task finish and the gauge is read on a quiet node.
const SETTLE_STEPS: u64 = 256;

/// The leg's three cells, booted durable on the small mesh, writers none.
fn harness(seed: u64) -> DurableScenario {
    DurableScenario {
        cells: CELLS,
        always_writers: 0,
        esec_writers: 0,
        mem_writers: 0,
        ops_per_writer: 0,
        double_cut: false,
        ..DurableScenario::m2_durable(seed)
    }
}

/// Runs the leg; the node drops before the observer seals, so every cell's
/// census verdict is in the report.
pub(super) fn small_credit_leg(seed: u64) -> CapacityReport {
    let observer = TraceObserver::default();
    let mut violations = Violations::default();
    let mut readings = Vec::new();
    let steps = run(seed, &observer, &mut violations, &mut readings);
    let sealed = observer.seal();
    readings.push(("cap_rows_checked", sealed.census.rows_checked));
    readings.push(("cap_rows_crossed", sealed.census.rows_crossed));
    violations.extend(sealed.census.violations);
    CapacityReport {
        leg: CloseLeg::SmallCredit,
        trace_hash: hash64(&observer.trace_bytes(), seed),
        state_hash: sealed.state_hash,
        violations,
        scheduler_steps: steps,
        readings,
    }
}

/// The node of one run, its clock, disk and draws, for the leg's phases.
struct Run {
    node: Node,
    rng: SplitMix64,
    clock: Rc<VirtualClock>,
    disk: inf_log::fs::sim::SimDisk,
    step_ns_max: u64,
    steps: u64,
}

impl Run {
    fn step(&mut self) -> std::io::Result<()> {
        self.steps += 1;
        self.node.step(&mut self.rng, &self.clock, &self.disk, self.step_ns_max)
    }

    /// `argv` from a client of `cell`, stepped to its reply.
    fn call(&mut self, cell: usize, argv: &[&[u8]]) -> Result<Vec<u8>, String> {
        let mut client = MiniClient::connect(&mut self.node, cell);
        let reply = client.call(
            &mut self.node,
            &mut self.rng,
            &self.clock,
            &self.disk,
            self.step_ns_max,
            argv,
        );
        client.close(&mut self.node);
        match reply {
            Ok(Some(reply)) => Ok(reply),
            Ok(None) => Err(format!("STALL small-credit: {argv:?} unanswered")),
            Err(err) => Err(format!("small-credit: {argv:?} failed: {err}")),
        }
    }

    /// One `INFO tripwires` field of `cell`, read over the wire.
    fn tripwire(&mut self, cell: usize, field: &str) -> Result<u64, String> {
        let reply = self.call(cell, &[b"INFO", b"tripwires"])?;
        let text = String::from_utf8_lossy(&reply);
        let prefix = format!("{field}:");
        text.lines()
            .find_map(|line| line.strip_prefix(prefix.as_str()))
            .and_then(|value| value.trim().parse().ok())
            .ok_or_else(|| format!("small-credit: cell {cell}'s INFO has no `{field}`"))
    }
}

/// The leg's phases; returns the scheduler steps it took.
fn run(
    seed: u64,
    observer: &TraceObserver,
    violations: &mut Violations,
    readings: &mut Vec<(&'static str, u64)>,
) -> u64 {
    let scenario = harness(seed);
    let clock = Rc::new(VirtualClock::new(Nanos(1)));
    let disk = build_disk(seed, scenario.stall.as_ref());
    let node = match boot_meshed(
        &scenario,
        SMALL_CREDIT_MESH,
        PathBuf::from("node"),
        &disk,
        &clock,
        observer,
    ) {
        Ok(node) => node,
        Err(err) => {
            violations.push(format!("small-credit: boot failed: {err}"));
            return 0;
        }
    };
    let mut run = Run {
        node,
        rng: SplitMix64::new(seed ^ 0x5C3E_D17A),
        clock,
        disk,
        step_ns_max: scenario.step_ns_max,
        steps: 0,
    };
    if let Err(what) = traffic(&mut run, violations) {
        violations.push(what);
        return run.steps;
    }
    if let Err(what) = judge(&mut run, violations, readings) {
        violations.push(what);
    }
    run.steps
}

/// The namespace, then one `SET` toward Y from each of 8 clients of X, each
/// answered.
fn traffic(run: &mut Run, violations: &mut Violations) -> Result<(), String> {
    create_namespace(run)?;
    let mut clients = clients_using(run)?;
    let value = vec![b'v'; VALUE_BYTES];
    let names = names_on(Y, "sc", SETS);
    // One send each, no step between, each delivered whole at one reap:
    // X's pumps stage every `SET` they have credits for in one iteration, so
    // Y drains them in one batch.
    let delivery = run.node.nets[X].borrow_mut().deliver_whole();
    for (client, name) in clients.iter_mut().zip(&names) {
        client.send(&mut run.node, &[b"SET", name, &value]);
    }
    let mut replies: Vec<Option<Vec<u8>>> = vec![None; SETS];
    for _ in 0..STALL_STEPS {
        for (client, reply) in clients.iter_mut().zip(replies.iter_mut()) {
            if reply.is_none() {
                *reply = client.recv(&mut run.node);
            }
        }
        if replies.iter().all(Option::is_some) {
            break;
        }
        run.step().map_err(|err| format!("small-credit: step failed: {err}"))?;
    }
    run.node.nets[X].borrow_mut().restore_delivery(delivery);
    for client in clients {
        client.close(&mut run.node);
    }
    for (index, reply) in replies.iter().enumerate() {
        match reply {
            Some(reply) if reply == b"+OK\r\n" => {}
            Some(reply) => violations.push(format!(
                "small-credit: SET {index} read {:?}, want +OK",
                String::from_utf8_lossy(reply)
            )),
            None => {
                return Err(format!(
                    "STALL small-credit: SET {index} unanswered in {STALL_STEPS} steps"
                ));
            }
        }
    }
    for _ in 0..SETTLE_STEPS {
        run.step().map_err(|err| format!("small-credit: step failed: {err}"))?;
    }
    Ok(())
}

/// `INF.NS CREATE` of the tiered `always` namespace, from a client of X.
fn create_namespace(run: &mut Run) -> Result<(), String> {
    let create: &[&[u8]] = &[
        b"INF.NS",
        b"CREATE",
        NS_NAME,
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
        b"TIER-IO-MODE",
        b"buffered",
    ];
    let created = run.call(X, create)?;
    if created != b"+OK\r\n" {
        let created = String::from_utf8_lossy(&created);
        return Err(format!("small-credit: CREATE answered {created:?}"));
    }
    Ok(())
}

/// 8 clients of X, each in the namespace: a connection's pump sends its
/// namespace applies one at a time, so the origin's batch is one `SET` from
/// each.
fn clients_using(run: &mut Run) -> Result<Vec<MiniClient>, String> {
    let mut clients = Vec::new();
    for _ in 0..SETS {
        let mut client = MiniClient::connect(&mut run.node, X);
        let used = client.call(
            &mut run.node,
            &mut run.rng,
            &run.clock,
            &run.disk,
            run.step_ns_max,
            &[b"INF.NS", b"USE", NS_NAME],
        );
        match used {
            Ok(Some(reply)) if reply == b"+OK\r\n" => clients.push(client),
            other => return Err(format!("small-credit: INF.NS USE answered {other:?}")),
        }
    }
    Ok(clients)
}

/// The gauge's clauses on Y, read over the wire on the quiet node, and the
/// derived-never refusals on every cell.
fn judge(
    run: &mut Run,
    violations: &mut Violations,
    readings: &mut Vec<(&'static str, u64)>,
) -> Result<(), String> {
    let parked_max = run.tripwire(Y, "fabric_parked_bytes_max")?;
    let parked_quiet = run.tripwire(Y, "fabric_parked_bytes")?;
    let mut refused = 0;
    for cell in 0..usize::from(CELLS) {
        refused += run.tripwire(cell, "fabric_parked_refused")?;
    }
    let applies_high_water =
        run.node.plane(Y).cap_census().row("ns-applies").map_or(0, |row| u64::from(row.high_water));
    readings.push(("ns_applies_high_water", applies_high_water));
    readings.push(("fabric_parked_bytes_max", parked_max));
    readings.push(("fabric_parked_bytes_quiet", parked_quiet));
    readings.push(("fabric_parked_refused", refused));
    if parked_max == 0 {
        violations.push(
            "VACUOUS small-credit: parked-bytes-never-parked — cell Y parked no byte of X's \
             tiered applies"
                .into(),
        );
    }
    if parked_quiet != 0 {
        violations.push(format!(
            "small-credit: parked-bytes-kept — cell Y's fabric_parked_bytes reads {parked_quiet} \
             with every reply in"
        ));
    }
    if refused != 0 {
        violations.push(format!(
            "small-credit: parked-refused — {refused} op(s) refused at a full per-origin deque, \
             derived never"
        ));
    }
    Ok(())
}
