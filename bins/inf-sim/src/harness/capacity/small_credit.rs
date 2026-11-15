//! `m0-capacity`'s small-credit leg: the work one cell parks for a peer
//! (ADR-0177 D6), on a mesh of 8 data credits a peer. 8 clients of X each
//! send one `SET` of a 4,096 B value, to a key Y owns, in a tiered `always`
//! namespace, in one step, so each apply reaches Y's fabric drain as a
//! tiered apply that Y diverts to the apply pump it makes on demand for X,
//! and each verdict waits in Y's gated deque for X until Y's watermark covers
//! it. A general send keeps one credit free for the publish fan (ADR-0192
//! D2), so 7 applies reach Y in one batch, the 8th after a reply. The keys
//! are of one length, so each parked apply's flat copy is the same `E`
//! bytes, the harness's arithmetic over the argv it sent. Read on Y once
//! every reply is in, each clause its token: every `SET` answers `+OK`;
//! Y's apply deque for X held 7 at its high water and crossed nothing
//! (`applies-high-water`, `applies-crossed`; below 7 is `VACUOUS`, the batch
//! split); its parked bytes' high water is `7 × E` (`parked-bytes-max`);
//! its parked bytes are back at 0 (`parked-bytes-kept`); no op was refused
//! at a full deque on any cell (`parked-refused`). The census reads every
//! row's high water within its cap when the node drops.

use std::path::PathBuf;
use std::rc::Rc;

use inf_foundation::hash64;
use inf_foundation::rng::SplitMix64;
use inf_foundation::time::{Nanos, VirtualClock};

use inf_store::SlotRouter;

use super::{CELLS, CapacityReport, CloseLeg, X, Y};
use crate::driver::Violations;
use crate::durable::{
    DurableScenario, MiniClient, Node, STALL_STEPS, TraceObserver, boot_meshed, build_disk,
};

/// A mesh of 8 data credits a peer (the leg's `d`), on rings with the
/// reply headroom the plane's mesh check reads.
pub(super) const SMALL_CREDIT_MESH: inf_fabric::MeshConfig = crate::harness::scenario_mesh(1024, 8);
/// The un-replied general ops an origin holds toward a peer at `d` = 8: one
/// credit stays free for the publish fan (ADR-0192 D2).
pub(super) const GENERAL_WINDOW: u64 = 7;
/// The tiered namespace the leg writes.
const NS_NAME: &[u8] = b"sc";
/// The `SET`s X pipelines toward Y, and each value's bytes.
const SETS: usize = 8;
const VALUE_BYTES: usize = 4_096;
/// Steps the node takes once every reply is in, so the pumps and the
/// `gated` task finish and the gauge is read on a quiet node.
pub(super) const SETTLE_STEPS: u64 = 256;

/// `cells` cells booted durable on the small mesh, writers none.
fn harness(seed: u64, cells: u16) -> DurableScenario {
    DurableScenario {
        cells,
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

/// `count` names of one length that cell `owner` of a `cells`-cell node
/// owns: `{stem}:NN`, the number zero-padded to two digits.
pub(super) fn padded_names_on(cells: u16, owner: usize, stem: &str, count: usize) -> Vec<Vec<u8>> {
    let router = SlotRouter::new_contiguous(cells);
    let names: Vec<Vec<u8>> = (0..100)
        .map(|n| format!("{stem}:{n:02}").into_bytes())
        .filter(|name| usize::from(router.cell_of(SlotRouter::slot_of(name)).0) == owner)
        .take(count)
        .collect();
    assert_eq!(names.len(), count, "{count} two-digit names of {stem} on cell {owner}");
    names
}

/// A parked apply's bytes: the flat copy of its argv, `[argc:u32]`, an end
/// `u32` an argument, then the arguments.
fn flat_bytes(argv: &[&[u8]]) -> u64 {
    let args: usize = argv.iter().map(|arg| arg.len()).sum();
    (4 + 4 * argv.len() + args) as u64
}

/// The node of one run, its clock, disk and draws, for a leg's phases.
pub(super) struct Run {
    pub(super) node: Node,
    pub(super) rng: SplitMix64,
    pub(super) clock: Rc<VirtualClock>,
    pub(super) disk: inf_log::fs::sim::SimDisk,
    pub(super) step_ns_max: u64,
    pub(super) steps: u64,
}

impl Run {
    /// `cells` cells at `seed` on the small mesh, observed by `observer`.
    pub(super) fn boot(seed: u64, cells: u16, observer: &TraceObserver) -> Result<Run, String> {
        let scenario = harness(seed, cells);
        let clock = Rc::new(VirtualClock::new(Nanos(1)));
        let disk = build_disk(seed, scenario.stall.as_ref());
        let node = boot_meshed(
            &scenario,
            SMALL_CREDIT_MESH,
            PathBuf::from("node"),
            &disk,
            &clock,
            observer,
        )
        .map_err(|err| format!("boot failed: {err}"))?;
        Ok(Run {
            node,
            rng: SplitMix64::new(seed ^ 0x5C3E_D17A),
            clock,
            disk,
            step_ns_max: scenario.step_ns_max,
            steps: 0,
        })
    }

    pub(super) fn step(&mut self) -> std::io::Result<()> {
        self.steps += 1;
        self.node.step(&mut self.rng, &self.clock, &self.disk, self.step_ns_max)
    }

    /// `argv` from `client`, stepped to its reply.
    pub(super) fn call_on(
        &mut self,
        client: &mut MiniClient,
        argv: &[&[u8]],
    ) -> Result<Vec<u8>, String> {
        let reply = client.call(
            &mut self.node,
            &mut self.rng,
            &self.clock,
            &self.disk,
            self.step_ns_max,
            argv,
        );
        match reply {
            Ok(Some(reply)) => Ok(reply),
            Ok(None) => Err(format!("STALL {argv:?} unanswered")),
            Err(err) => Err(format!("{argv:?} failed: {err}")),
        }
    }

    /// `argv` from a client of `cell`, stepped to its reply.
    pub(super) fn call(&mut self, cell: usize, argv: &[&[u8]]) -> Result<Vec<u8>, String> {
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

    /// A client of `cell` in namespace `name`.
    pub(super) fn client_in(&mut self, cell: usize, name: &[u8]) -> Result<MiniClient, String> {
        let mut client = MiniClient::connect(&mut self.node, cell);
        match self.call_on(&mut client, &[b"INF.NS", b"USE", name])?.as_slice() {
            b"+OK\r\n" => Ok(client),
            other => Err(format!("INF.NS USE answered {:?}", String::from_utf8_lossy(other))),
        }
    }

    /// One `INFO tripwires` field of `cell`, read over the wire.
    pub(super) fn tripwire(&mut self, cell: usize, field: &str) -> Result<u64, String> {
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
    let mut run = match Run::boot(seed, CELLS, observer) {
        Ok(run) => run,
        Err(what) => {
            violations.push(format!("small-credit: {what}"));
            return 0;
        }
    };
    let entry = match traffic(&mut run, violations) {
        Ok(entry) => entry,
        Err(what) => {
            violations.push(format!("small-credit: {what}"));
            return run.steps;
        }
    };
    if let Err(what) = judge(&mut run, entry, violations, readings) {
        violations.push(format!("small-credit: {what}"));
    }
    run.steps
}

/// The namespace, then one `SET` toward Y from each of 8 clients of X, each
/// answered. Returns one parked apply's bytes, `E`.
fn traffic(run: &mut Run, violations: &mut Violations) -> Result<u64, String> {
    create_namespace(run)?;
    let mut clients = Vec::new();
    for _ in 0..SETS {
        // A connection's pump sends its namespace applies one at a time, so
        // the origin's batch is one `SET` from each of 8 connections.
        clients.push(run.client_in(X, NS_NAME)?);
    }
    let value = vec![b'v'; VALUE_BYTES];
    let names = padded_names_on(CELLS, Y, "sc", SETS);
    let entry = flat_bytes(&[b"SET", &names[0], &value]);
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
        run.step().map_err(|err| format!("step failed: {err}"))?;
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
            None => return Err(format!("STALL SET {index} unanswered in {STALL_STEPS} steps")),
        }
    }
    for _ in 0..SETTLE_STEPS {
        run.step().map_err(|err| format!("step failed: {err}"))?;
    }
    Ok(entry)
}

/// `INF.NS CREATE` of the tiered `always` namespace, from a client of X.
fn create_namespace(run: &mut Run) -> Result<(), String> {
    let created = run.call(X, &tiered_create(NS_NAME))?;
    if created != b"+OK\r\n" {
        let created = String::from_utf8_lossy(&created);
        return Err(format!("CREATE answered {created:?}"));
    }
    Ok(())
}

/// The `INF.NS CREATE` of a tiered `always` namespace `name`: a 3 MiB
/// window, all of it mutable, flushed in 1 MiB slices to buffered tier
/// files.
pub(super) fn tiered_create(name: &[u8]) -> Vec<&[u8]> {
    vec![
        b"INF.NS",
        b"CREATE",
        name,
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
    ]
}

/// The clauses on Y, read on the quiet node: the census row of Y's apply
/// deques and, over the wire, the gauges.
fn judge(
    run: &mut Run,
    entry: u64,
    violations: &mut Violations,
    readings: &mut Vec<(&'static str, u64)>,
) -> Result<(), String> {
    let parked_max = run.tripwire(Y, "fabric_parked_bytes_max")?;
    let parked_quiet = run.tripwire(Y, "fabric_parked_bytes")?;
    let mut refused = 0;
    for cell in 0..usize::from(CELLS) {
        refused += run.tripwire(cell, "fabric_parked_refused")?;
    }
    let row = run.node.plane(Y).cap_census().row("ns-applies");
    let (applies, crossed) = row.map_or((0, 0), |row| (u64::from(row.high_water), row.crossings));
    readings.push(("ns_applies_high_water", applies));
    readings.push(("ns_applies_crossings", crossed));
    readings.push(("fabric_parked_bytes_max", parked_max));
    readings.push(("fabric_parked_bytes_quiet", parked_quiet));
    readings.push(("fabric_parked_refused", refused));
    if applies < GENERAL_WINDOW {
        violations.push(format!(
            "VACUOUS small-credit: applies-high-water — Y's apply deque for X held {applies}, \
             under the {GENERAL_WINDOW} one batch carries: the batch split"
        ));
    } else if applies > GENERAL_WINDOW {
        violations.push(format!(
            "small-credit: applies-high-water — Y's apply deque for X held {applies}, over the \
             {GENERAL_WINDOW} general ops an origin holds toward a peer"
        ));
    }
    if crossed != 0 {
        violations
            .push(format!("small-credit: applies-crossed — the row crossed {crossed} time(s)"));
    }
    let want = GENERAL_WINDOW * entry;
    if parked_max != want {
        violations.push(format!(
            "small-credit: parked-bytes-max — Y's parked bytes peaked at {parked_max}, want \
             {GENERAL_WINDOW} applies of {entry} B, {want}"
        ));
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
