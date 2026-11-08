//! `m4-ns-lifetimes` — tiered namespace lifetimes (ADR-0148): a node holds
//! at most 64 tiered namespace lifetimes, a dropped one counted until its
//! runtime owners have ended on every cell. Past that a tiered `CREATE` is
//! refused before any effect, `-TRYAGAIN tiered namespace capacity in use`
//! while a dropped one's owners are still ending and `-ERR tiered namespace
//! limit reached` when every grant names a live namespace; capacity comes
//! back once every cell's receipt names the dropped id.
//!
//! One seeded run on a two- or four-cell durable node (the seed's parity),
//! its device's barriers slow enough (20 ms an fsync, steps of at most 1 ms)
//! that a blob write's extent barrier outlives a few steps:
//! 1. boot; 64 tiered `CREATE`s, each `+OK`, then the 65th refused at the
//!    limit;
//! 2. the churn: `CHURN` cycles, each dropping the namespace the last one
//!    created while blob writes on it are in flight on every cell (their
//!    extent barriers hold its owners), then a tiered `CREATE` retried
//!    until `+OK`: refused while the dropped grant is Retiring, accepted
//!    once every receipt is in, its position the dropped one's;
//! 3. the disconnect: a tiered `CREATE` whose client closes after its
//!    request is queued, the writer held: the program runs to its end, the
//!    namespace on every cell, its id the last committed + 1;
//! 4. the peer refusal at 63 grants: a tiered `CREATE` whose leg one peer
//!    refuses is rolled back on every cell; the next `CREATE` is accepted
//!    once every receipt is in, its id the last `+OK`'s + 2;
//! 5. a power cut and a reboot: every live name served on every cell, the
//!    refused one on none.
//!
//! Oracles: `LIFETIMES` (O1), every step, from the sim disk's own tables:
//! under every shard at most 64 `ns-<id>` directories hold a file an open
//! description names. `CUSTODY` (O2), every step: no driver op named a
//! closed descriptor. `IDS` (O4): each `+OK` `CREATE`'s id (`INF.NS INFO`)
//! is the last committed + 1, a rolled-back one committing one. `REPLY`
//! (O5): every `CREATE` answers `+OK`, the taken-name error, a tiered or a
//! catalog capacity line, never an internal error.

use std::path::PathBuf;
use std::rc::Rc;

use inf_foundation::fault::FaultSpec;
use inf_foundation::hash64;
use inf_foundation::rng::{Entropy, SplitMix64};
use inf_foundation::time::{Clock, Nanos, VirtualClock};
use inf_log::fs::sim::StallConfig;

use crate::durable::{
    DurableScenario, MiniClient, Node, STALL_STEPS, TraceObserver, boot, build_disk,
    m2_stall_config,
};

const NS_CREATE_FAN_REFUSED: &str = "ns_create_fan_refused";
/// Tiered namespace lifetimes a node holds (ADR-0148 A1.1's limit).
const LIFETIMES: usize = 64;
/// Drop-and-create cycles of phase 2.
const CHURN: u32 = 6;
/// Bound on the retries of one tiered `CREATE` refused while a grant is
/// Retiring (each retry waits `RETRY_GAP_STEPS`).
const RETRIES_MAX: u32 = 400;
const RETRY_GAP_STEPS: u32 = 4;
/// Blob writes in flight on each cell at a drop (6 KiB values, above the
/// 4 KiB `BLOB-THRESHOLD`).
const BLOBS_PER_CELL: usize = 4;

const OK: &[u8] = b"+OK\r\n";
const LIMIT: &[u8] = b"-ERR tiered namespace limit reached\r\n";
const BUSY: &[u8] = b"-TRYAGAIN tiered namespace capacity in use\r\n";
/// Every reply a `CREATE` may answer here (O5).
const ALLOWED: [&[u8]; 6] = [
    OK,
    b"-ERR namespace already exists\r\n",
    BUSY,
    LIMIT,
    b"-TRYAGAIN catalog capacity in use\r\n",
    b"-ERR catalog limit reached\r\n",
];

/// The run's verdict and coverage disclosures.
#[derive(Debug)]
pub struct NsLifetimesReport {
    pub trace: Vec<u8>,
    pub trace_hash: u64,
    pub state_hash: u64,
    pub violations: Vec<String>,
    /// Cap census rows read at the end of every life (node sum).
    pub cap_rows_checked: u64,
    /// Census rows the crossing rule judged (their `crossings > 0`).
    pub cap_rows_crossed: u64,
    pub stalled: bool,
    pub commands_done: u64,
    pub scheduler_steps: u64,
    pub sim_seconds: f64,
    pub cells: u16,
    /// `CREATE`s answered `+OK`.
    pub creates_ok: u64,
    /// Tiered `CREATE`s refused while a grant was Retiring, and at the limit.
    pub refused_busy: u64,
    pub refused_limit: u64,
    /// The most `ns-<id>` directories holding an open file under one shard.
    pub open_dirs_max: usize,
    /// The peer refusal's leg (1-based over the peers).
    pub refused_leg: u64,
}

impl NsLifetimesReport {
    #[must_use]
    pub fn ok(&self) -> bool {
        !self.stalled && self.violations.is_empty()
    }
}

/// The node, its clocks and the oracles' state.
struct Run {
    node: Node,
    rng: SplitMix64,
    clock: Rc<VirtualClock>,
    disk: inf_server::SimDisk,
    step_ns_max: u64,
    report: NsLifetimesReport,
    /// The last namespace id committed (O4).
    last_committed: u32,
}

impl Run {
    fn fail(&mut self, message: String) {
        if self.report.violations.len() < 16 {
            self.report.violations.push(message);
        }
    }

    /// One scheduler step, then O1 and O2 on the disk's own tables.
    fn step(&mut self) -> Result<(), String> {
        self.report.scheduler_steps += 1;
        self.node
            .step(&mut self.rng, &self.clock, &self.disk, self.step_ns_max)
            .map_err(|e| format!("step failed: {e}"))?;
        for (shard, dirs) in self.disk.open_ns_dirs() {
            self.report.open_dirs_max = self.report.open_dirs_max.max(dirs);
            if dirs > LIFETIMES {
                self.fail(format!(
                    "LIFETIMES: {dirs} ns directories hold open files under {shard}, above \
                     {LIFETIMES}"
                ));
            }
        }
        let closed = self.disk.closed_fd_ops();
        if closed > 0 {
            self.fail(format!("CUSTODY: {closed} driver ops named a closed descriptor"));
        }
        Ok(())
    }

    /// `client`'s next reply, stepping until it arrives (bounded).
    fn reply(&mut self, client: &mut MiniClient) -> Result<Vec<u8>, String> {
        for _ in 0..STALL_STEPS {
            if let Some(reply) = client.recv(&mut self.node) {
                self.report.commands_done += 1;
                return Ok(reply);
            }
            self.step()?;
        }
        self.report.stalled = true;
        Err("a reply never arrived".to_owned())
    }

    fn call(&mut self, client: &mut MiniClient, argv: &[&[u8]]) -> Result<Vec<u8>, String> {
        client.send(&mut self.node, argv);
        self.reply(client)
    }

    /// `INF.NS INFO name`'s `id`.
    fn ns_id(&mut self, client: &mut MiniClient, name: &[u8]) -> Result<u32, String> {
        let frame = self.call(client, &[b"INF.NS", b"INFO", name])?;
        let text = String::from_utf8_lossy(&frame).into_owned();
        let lines: Vec<&str> = text.split("\r\n").collect();
        let at = lines.iter().position(|line| *line == "id").ok_or(format!("no id in {text:?}"))?;
        lines.get(at + 2).and_then(|id| id.parse().ok()).ok_or(format!("no id in {text:?}"))
    }

    /// O4 for an accepted `CREATE` of `name`.
    fn accepted(&mut self, client: &mut MiniClient, name: &[u8]) -> Result<(), String> {
        let id = self.ns_id(client, name)?;
        if id != self.last_committed + 1 {
            self.fail(format!(
                "IDS: CREATE {} took id {id} after the last committed {}",
                String::from_utf8_lossy(name),
                self.last_committed
            ));
        }
        self.last_committed = id;
        self.report.creates_ok += 1;
        Ok(())
    }

    /// One tiered `CREATE` of `name` and its reply, O5 judged.
    fn create(&mut self, client: &mut MiniClient, name: &[u8]) -> Result<Vec<u8>, String> {
        let argv: [&[u8]; 9] = [
            b"INF.NS",
            b"CREATE",
            name,
            b"MODE",
            b"durable",
            b"MEM-BUDGET",
            b"3mb",
            b"BLOB-THRESHOLD",
            b"4kb",
        ];
        let reply = self.call(client, &argv)?;
        if !ALLOWED.contains(&reply.as_slice()) {
            self.fail(format!(
                "REPLY: CREATE {} answered {:?}",
                String::from_utf8_lossy(name),
                String::from_utf8_lossy(&reply)
            ));
        }
        if reply == OK {
            self.accepted(client, name)?;
        }
        Ok(reply)
    }

    /// A tiered `CREATE` of `name` retried while a grant is Retiring, until
    /// it is accepted (bounded).
    fn create_until_ok(&mut self, client: &mut MiniClient, name: &[u8]) -> Result<(), String> {
        for _ in 0..RETRIES_MAX {
            let reply = self.create(client, name)?;
            if reply == OK {
                return Ok(());
            }
            if reply != BUSY {
                return Err(format!(
                    "CREATE {} answered {:?} while a grant returns",
                    String::from_utf8_lossy(name),
                    String::from_utf8_lossy(&reply)
                ));
            }
            self.report.refused_busy += 1;
            for _ in 0..RETRY_GAP_STEPS {
                self.step()?;
            }
        }
        self.report.stalled = true;
        Err(format!(
            "CREATE {} never accepted: the grant never returned",
            String::from_utf8_lossy(name)
        ))
    }

    /// `INF.NS DROP name`, answered `+OK`.
    fn drop_ns(&mut self, client: &mut MiniClient, name: &[u8]) -> Result<(), String> {
        let reply = self.call(client, &[b"INF.NS", b"DROP", name])?;
        if reply != OK {
            return Err(format!("DROP {} answered {reply:?}", String::from_utf8_lossy(name)));
        }
        Ok(())
    }

    /// The cells that answer `+OK` to `INF.NS USE name`, one fresh client
    /// each.
    fn served_on(&mut self, name: &[u8]) -> Result<Vec<u16>, String> {
        let mut served = Vec::new();
        for cell in 0..self.report.cells {
            let mut probe = MiniClient::connect(&mut self.node, usize::from(cell));
            let reply = self.call(&mut probe, &[b"INF.NS", b"USE", name])?;
            if reply == OK {
                served.push(cell);
            } else if !reply.starts_with(b"-ERR namespace") {
                return Err(format!("cell {cell}: USE answered {reply:?}"));
            }
        }
        Ok(served)
    }

    /// Steps until every cell is ready (bounded).
    fn recover(&mut self) -> Result<(), String> {
        for _ in 0..STALL_STEPS {
            if self.node.ready() {
                return Ok(());
            }
            self.step()?;
        }
        self.report.stalled = true;
        Err("recovery stalled".to_owned())
    }

    /// Steps `steps` times.
    fn idle(&mut self, steps: u32) -> Result<(), String> {
        for _ in 0..steps {
            self.step()?;
        }
        Ok(())
    }
}

/// The scenario's node: the m2 durable shape on two or four cells, 64 KiB
/// segments, a device whose fsyncs take 20 ms against steps of at most 1 ms.
fn scenario(seed: u64) -> DurableScenario {
    let stall = StallConfig { base_ns: 20_000_000, ..m2_stall_config() };
    DurableScenario {
        cells: if seed.is_multiple_of(2) { 2 } else { 4 },
        segment_bytes: 64 << 10,
        step_ns_max: 1_000_000,
        stall: Some(stall),
        ..DurableScenario::m2_durable(seed)
    }
}

fn name(prefix: &str, index: u32) -> Vec<u8> {
    format!("{prefix}{index:02}").into_bytes()
}

/// Phase 1: 64 tiered `CREATE`s accepted, the 65th refused at the limit.
fn fill(run: &mut Run, admin: &mut MiniClient) -> Result<(), String> {
    for index in 0..LIFETIMES as u32 {
        let reply = run.create(admin, &name("t", index))?;
        if reply != OK {
            return Err(format!("tiered CREATE {index} below the limit answered {reply:?}"));
        }
    }
    let reply = run.create(admin, b"over")?;
    if reply == LIMIT {
        run.report.refused_limit += 1;
    } else {
        run.fail(format!("VACUOUS: the 65th tiered CREATE answered {reply:?}, not the limit"));
    }
    Ok(())
}

/// Phase 2: each cycle drops `victim` with blob writes in flight on every
/// cell, then creates its successor, retried while the grant is Retiring.
/// Returns the last namespace created.
fn churn(run: &mut Run, admin: &mut MiniClient, mut victim: Vec<u8>) -> Result<Vec<u8>, String> {
    let value = vec![b'b'; 6 << 10];
    for cycle in 0..CHURN {
        let mut writers = Vec::new();
        for cell in 0..run.report.cells {
            let mut writer = MiniClient::connect(&mut run.node, usize::from(cell));
            let used = run.call(&mut writer, &[b"INF.NS", b"USE", &victim])?;
            if used != OK {
                return Err(format!("USE {} answered {used:?}", String::from_utf8_lossy(&victim)));
            }
            for blob in 0..BLOBS_PER_CELL {
                let key = format!("blob:{cycle}:{cell}:{blob}").into_bytes();
                writer.send(&mut run.node, &[b"SET", &key, &value]);
            }
            writers.push(writer);
        }
        // The drop's legs meet the writes' extent barriers still out, and
        // the successor's CREATE goes at once: its grant waits for them.
        run.drop_ns(admin, &victim)?;
        let next = name("n", cycle);
        run.create_until_ok(admin, &next)?;
        for mut writer in writers {
            for _ in 0..BLOBS_PER_CELL {
                run.reply(&mut writer)?;
            }
        }
        victim = next;
    }
    if run.report.refused_busy == 0 {
        run.fail("VACUOUS: no tiered CREATE met a Retiring grant".to_owned());
    }
    Ok(victim)
}

/// Waits until a tiered `CREATE` of `probe` is accepted, then drops it:
/// the node back at 63 Declared grants, none Retiring for long.
fn settle_at_63(run: &mut Run, admin: &mut MiniClient, probe: &[u8]) -> Result<(), String> {
    run.create_until_ok(admin, probe)?;
    run.drop_ns(admin, probe)?;
    run.idle(64)
}

/// Phase 3: a tiered `CREATE` whose client closes after its request is
/// queued, the writer held: the program runs to its end.
fn disconnect(run: &mut Run, admin: &mut MiniClient) -> Result<(), String> {
    let cell = usize::from(run.report.cells - 1);
    let mut client = MiniClient::connect(&mut run.node, cell);
    let sent = run.node.control.persist_requests_sent();
    run.node.hold_inbox = true;
    let argv: [&[u8]; 7] =
        [b"INF.NS", b"CREATE", b"d00", b"MODE", b"durable", b"MEM-BUDGET", b"3mb"];
    client.send(&mut run.node, &argv);
    let mut queued = false;
    for _ in 0..STALL_STEPS {
        run.step()?;
        if run.node.control.persist_requests_sent() > sent {
            queued = true;
            break;
        }
    }
    if !queued {
        return Err("VACUOUS: the disconnected CREATE's request was never queued".to_owned());
    }
    client.close(&mut run.node);
    run.node.hold_inbox = false;
    for _ in 0..STALL_STEPS {
        run.step()?;
        if run.node.control.ddl_holder().is_none() {
            break;
        }
    }
    if run.node.control.ddl_holder().is_some() {
        run.report.stalled = true;
        return Err("the disconnected CREATE's program never ended".to_owned());
    }
    run.accepted(admin, b"d00")?;
    let served = run.served_on(b"d00")?;
    if served.len() != usize::from(run.report.cells) {
        run.fail(format!("a disconnected CREATE is served on cells {served:?} only"));
    }
    run.drop_ns(admin, b"d00")?;
    run.idle(64)
}

/// Phase 4: the peer refusal at 63 grants.
fn peer_refusal(run: &mut Run, admin: &mut MiniClient) -> Result<(), String> {
    let refused_leg = 1 + run.rng.next_below(u64::from(run.report.cells) - 1);
    run.report.refused_leg = refused_leg;
    inf_foundation::fault::arm(NS_CREATE_FAN_REFUSED, FaultSpec::Nth(refused_leg));
    let argv: [&[u8]; 7] =
        [b"INF.NS", b"CREATE", b"r00", b"MODE", b"durable", b"MEM-BUDGET", b"3mb"];
    let reply = run.call(admin, &argv)?;
    inf_foundation::fault::disarm(NS_CREATE_FAN_REFUSED);
    if reply != b"-ERR fault: ns_create_fan_refused\r\n" {
        return Err(format!("VACUOUS: the refused leg's CREATE answered {reply:?}"));
    }
    // The writer committed its id at the acceptance the rollback followed.
    run.last_committed += 1;
    let served = run.served_on(b"r00")?;
    if !served.is_empty() {
        run.fail(format!("a CREATE refused at a peer's leg is served on cells {served:?}"));
    }
    run.create_until_ok(admin, b"p00")
}

/// Runs one seeded `m4-ns-lifetimes` scenario.
fn run_observed(seed: u64, observer: TraceObserver) -> NsLifetimesReport {
    let scenario = scenario(seed);
    let clock = Rc::new(VirtualClock::new(Nanos(1)));
    let disk = build_disk(seed, scenario.stall.as_ref());
    let report = NsLifetimesReport {
        trace: Vec::new(),
        trace_hash: 0,
        state_hash: 0,
        violations: Vec::new(),
        cap_rows_checked: 0,
        cap_rows_crossed: 0,
        stalled: false,
        commands_done: 0,
        scheduler_steps: 0,
        sim_seconds: 0.0,
        cells: scenario.cells,
        creates_ok: 0,
        refused_busy: 0,
        refused_limit: 0,
        open_dirs_max: 0,
        refused_leg: 0,
    };
    let node = match boot(&scenario, PathBuf::from("node"), &disk, &clock, &observer) {
        Ok(node) => node,
        Err(err) => {
            let mut report = report;
            report.violations.push(format!("boot failed: {err}"));
            return finish(report, &observer, &clock);
        }
    };
    let mut run = Run {
        node,
        rng: SplitMix64::new(seed ^ 0x11FE_7135),
        clock: Rc::clone(&clock),
        disk: disk.clone(),
        step_ns_max: scenario.step_ns_max,
        report,
        last_committed: inf_store::FIRST_NAMED_NS_ID - 1,
    };
    if let Err(err) = phases(&mut run) {
        run.fail(err);
        let Run { report, .. } = run;
        return finish(report, &observer, &clock);
    }
    // The cut and the reboot: the old node is gone before the disk is cut.
    let Run { node, rng, clock: run_clock, disk, step_ns_max, report, last_committed } = run;
    drop(node);
    observer.power_cut(&disk, run_clock.now(), seed ^ 0x11FE_0C07);
    let node = match boot(&scenario, PathBuf::from("node"), &disk, &run_clock, &observer) {
        Ok(node) => node,
        Err(err) => {
            let mut report = report;
            report.violations.push(format!("reboot after the cut refused: {err}"));
            return finish(report, &observer, &clock);
        }
    };
    let mut run = Run { node, rng, clock: run_clock, disk, step_ns_max, report, last_committed };
    if let Err(err) = after_cut(&mut run) {
        run.fail(err);
    }
    let Run { report, .. } = run;
    finish(report, &observer, &clock)
}

/// Phases 1–4 on one life.
fn phases(run: &mut Run) -> Result<(), String> {
    run.recover()?;
    let mut admin = MiniClient::connect(&mut run.node, 0);
    fill(run, &mut admin)?;
    let last = churn(run, &mut admin, b"t00".to_vec())?;
    run.drop_ns(&mut admin, &last)?;
    settle_at_63(run, &mut admin, b"s00")?;
    disconnect(run, &mut admin)?;
    peer_refusal(run, &mut admin)
}

/// Phase 5, after the cut: every live name on every cell, the refused one
/// on none, and a `CREATE` after a drop taking the next id.
fn after_cut(run: &mut Run) -> Result<(), String> {
    run.recover()?;
    let live: Vec<Vec<u8>> = (1..LIFETIMES as u32)
        .map(|index| name("t", index))
        .chain(std::iter::once(b"p00".to_vec()))
        .collect();
    for name in &live {
        let served = run.served_on(name)?;
        if served.len() != usize::from(run.report.cells) {
            run.fail(format!(
                "after the cut {} is served on cells {served:?} only",
                String::from_utf8_lossy(name)
            ));
        }
    }
    let refused = run.served_on(b"r00")?;
    if !refused.is_empty() {
        run.fail(format!("after the cut the refused CREATE is served on cells {refused:?}"));
    }
    let mut admin = MiniClient::connect(&mut run.node, 0);
    run.drop_ns(&mut admin, b"p00")?;
    run.create_until_ok(&mut admin, b"z00")?;
    if run.report.open_dirs_max == 0 {
        run.fail("VACUOUS: no ns directory held an open file".to_owned());
    }
    Ok(())
}

fn finish(
    mut report: NsLifetimesReport,
    observer: &TraceObserver,
    clock: &Rc<VirtualClock>,
) -> NsLifetimesReport {
    report.trace = observer.trace_bytes();
    report.trace_hash = hash64(&report.trace, 0x11FE);
    report.sim_seconds = clock.now().0.saturating_sub(1) as f64 / 1e9;
    report
}

/// Runs the scenario and seals state evidence after every node has been
/// dropped.
#[must_use]
pub fn run_ns_lifetimes_scenario(seed: u64) -> NsLifetimesReport {
    let observer = TraceObserver::default();
    let mut report = run_observed(seed, observer.clone());
    let sealed = observer.seal();
    report.state_hash = sealed.state_hash;
    report.cap_rows_checked = sealed.census.rows_checked;
    report.cap_rows_crossed = sealed.census.rows_crossed;
    report.violations.extend(sealed.census.violations);
    report
}
