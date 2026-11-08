//! `m2-ns-drop-window` — the namespace `DROP` window (ADR-0189 D5): no cell
//! applies a drop before the catalog writer's answer proves the `META` that
//! carries it durable, so a cut anywhere in a `DROP` loses no acknowledged
//! write and refuses no boot, and leaves the namespace whole on every cell
//! or gone from every cell; a lagging export never brings a dropped
//! namespace back; and at the tombstone cap the writer's own fold decides.
//! The mirror of `m2-ns-create-window`.
//!
//! One seeded run, in phases:
//! 1. **the stale fold:** boot a two-cell node on a `META` planted with 256
//!    tombstones and a durable `victim`; once every cell has published the
//!    boot's re-stamp, every tombstone is ready, so `DROP victim` at the cap
//!    issues its cleanup checkpoint alone;
//! 2. **the lagging arm:** a memory namespace dropped, and a memory `CREATE`
//!    rolled back (its peer leg refused), each with a lagging export sent
//!    inside its window; `META`, decoded from the disk after every step,
//!    never names either, and no notice retires another DDL;
//! 3. **the cut class** the seed picks, on a durable namespace with
//!    acknowledged keys on every cell: before the answer (the writer held, a
//!    checkpoint published on every cell inside the wait), after the answer
//!    (`ns_drop_after_meta`), after the origin's apply
//!    (`ns_drop_after_apply`), after the fan (the writer held from the
//!    answer on, so the notice never lands), or a fault point's return with
//!    the node running on (the next DDL answers);
//! 4. **the reboot:** it succeeds with no untombstoned skip, and the
//!    namespace is whole (every acknowledged key served) or gone, on every
//!    cell, as the cut's class says.
//!
//! Each class counts its own engagement (`held_steps`, `checkpoint_in_wait`),
//! and a cut that did not land where its class says is a violation.

use std::path::PathBuf;
use std::rc::Rc;

use inf_foundation::fault::FaultSpec;
use inf_foundation::hash64;
use inf_foundation::rng::SplitMix64;
use inf_foundation::time::{Clock, Nanos, VirtualClock};
use inf_log::fs::SegmentFs;
use inf_server::fault::{NS_CREATE_FAN_REFUSED, NS_DROP_AFTER_APPLY, NS_DROP_AFTER_META};
use inf_server::{CkptTarget, SimDisk, load_catalog_from};
use inf_store::{FsyncClass, IndexCatalog, NsCatalog, NsId, NsMode, NsSpec};

use crate::durable::{
    DurableScenario, MiniClient, Node, STALL_STEPS, TraceObserver, boot, build_disk,
};

/// Where the seed's cut lands in the `DROP` (ADR-0189 D5's cut table).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DropCut {
    /// Before the writer's answer: nothing applied, `META` old.
    BeforeAnswer,
    /// After the answer, before the origin's apply.
    AfterAnswer,
    /// After the origin's apply, before its first leg.
    AfterApply,
    /// After the fan, before the writer read the notice.
    AfterFan,
    /// A fault point's return with the node running on.
    FaultReturn,
}

impl DropCut {
    /// The seed's class: one of five, so five corpus seeds cover them all.
    #[must_use]
    pub fn of_seed(seed: u64) -> DropCut {
        match seed % 5 {
            0 => DropCut::BeforeAnswer,
            1 => DropCut::AfterAnswer,
            2 => DropCut::AfterApply,
            3 => DropCut::AfterFan,
            _ => DropCut::FaultReturn,
        }
    }
}

/// The run's verdict and coverage disclosures.
#[derive(Debug)]
pub struct NsDropWindowReport {
    pub trace: Vec<u8>,
    pub trace_hash: u64,
    pub state_hash: u64,
    pub violations: Vec<String>,
    /// Cap census rows read at the end of every life, summed (node sum).
    pub cap_rows_checked: u64,
    /// Census rows the crossing rule judged.
    pub cap_rows_crossed: u64,
    pub stalled: bool,
    pub commands_done: u64,
    pub scheduler_steps: u64,
    pub sim_seconds: f64,
    pub cut: DropCut,
    /// Checkpoint epochs the `DROP` at the cap issued (the stale fold: 1).
    pub stale_fold_epochs: u64,
    /// Lagging exports the writer answered inside a DDL's window.
    pub lagging_exports: u64,
    /// The class's engagement: steps the writer was held across the
    /// `DROP` (`BeforeAnswer`, `AfterFan`), or the fault point's reply
    /// (`AfterAnswer`, `AfterApply`, `FaultReturn`, one each).
    pub held_steps: u64,
    /// Whether every cell published a checkpoint inside the held wait.
    pub checkpoint_in_wait: bool,
    /// Acknowledged keys read back after the reboot.
    pub keys_served: u64,
    /// Untombstoned-unknown-namespace skips over every reboot and cell.
    pub skipped_unknown_ns: u64,
}

impl NsDropWindowReport {
    #[must_use]
    pub fn ok(&self) -> bool {
        !self.stalled && self.violations.is_empty()
    }
}

/// The run's clock, rng, disk and step bound, for the phase helpers.
struct World {
    seed: u64,
    rng: SplitMix64,
    clock: Rc<VirtualClock>,
    disk: SimDisk,
    step_ns_max: u64,
}

/// The data directory every life boots from.
fn data_dir() -> PathBuf {
    PathBuf::from("node")
}

/// A durable namespace's catalog entry.
fn durable_spec(id: u32, name: &[u8]) -> NsSpec {
    NsSpec {
        id: NsId(id),
        name: name.to_vec(),
        mode: NsMode::Durable,
        fsync: Some(FsyncClass::Always),
        policy: None,
        maxmemory: None,
        tier: None,
    }
}

impl World {
    fn step(&mut self, node: &mut Node, report: &mut NsDropWindowReport) -> Result<(), String> {
        report.scheduler_steps += 1;
        node.step(&mut self.rng, &self.clock, &self.disk, self.step_ns_max)
            .map_err(|e| format!("step failed: {e}"))
    }

    /// Steps until `done` holds, at most `STALL_STEPS` steps.
    fn step_until(
        &mut self,
        node: &mut Node,
        report: &mut NsDropWindowReport,
        what: &str,
        done: impl Fn(&Node) -> bool,
    ) -> Result<u64, String> {
        let mut steps = 0u64;
        while !done(node) {
            self.step(node, report)?;
            steps += 1;
            if steps > STALL_STEPS {
                report.stalled = true;
                return Err(format!("stalled waiting for {what}"));
            }
        }
        Ok(steps)
    }

    fn call(
        &mut self,
        node: &mut Node,
        report: &mut NsDropWindowReport,
        client: &mut MiniClient,
        argv: &[&[u8]],
    ) -> Result<Vec<u8>, String> {
        let replied =
            client.call(node, &mut self.rng, &self.clock, &self.disk, self.step_ns_max, argv);
        match replied {
            Ok(Some(reply)) => {
                report.commands_done += 1;
                Ok(reply)
            }
            Ok(None) => {
                report.stalled = true;
                Err(format!("{:?} stalled", words(argv)))
            }
            Err(e) => Err(format!("step failed: {e}")),
        }
    }

    /// `argv`, which must answer exactly `want`.
    fn expect(
        &mut self,
        node: &mut Node,
        report: &mut NsDropWindowReport,
        client: &mut MiniClient,
        argv: &[&[u8]],
        want: &[u8],
    ) -> Result<(), String> {
        let reply = self.call(node, report, client, argv)?;
        if reply != want {
            let (got, want) = (String::from_utf8_lossy(&reply), String::from_utf8_lossy(want));
            return Err(format!("{:?} answered {got:?}, not {want:?}", words(argv)));
        }
        Ok(())
    }

    /// `META` decoded from the disk, by the catalog's own decoder.
    fn meta(&self) -> Result<NsCatalog, String> {
        match load_catalog_from(&self.disk, &data_dir()) {
            Ok(Some(catalog)) => Ok(catalog),
            Ok(None) => Err("no META on the disk".to_owned()),
            Err(e) => Err(format!("META does not decode: {e}")),
        }
    }

    fn boot(&self, scenario: &DurableScenario, observer: &TraceObserver) -> Result<Node, String> {
        boot(scenario, data_dir(), &self.disk, &self.clock, observer)
            .map_err(|e| format!("boot refused: {e}"))
    }

    /// Boots and recovers; counts untombstoned-unknown skips as violations.
    fn boot_ready(
        &mut self,
        scenario: &DurableScenario,
        observer: &TraceObserver,
        report: &mut NsDropWindowReport,
    ) -> Result<Node, String> {
        let mut node = self.boot(scenario, observer)?;
        self.step_until(&mut node, report, "recovery", Node::ready)?;
        let skips: u64 = (0..2)
            .map(|c| node.control.recovery_board().slot(c).records_skipped_unknown_ns())
            .sum();
        report.skipped_unknown_ns += skips;
        if skips > 0 {
            let seed = self.seed;
            report.violations.push(format!("seed {seed:#x}: {skips} untombstoned unknown skips"));
        }
        Ok(node)
    }
}

/// An argv as text, for a violation's message.
fn words(argv: &[&[u8]]) -> Vec<String> {
    argv.iter().map(|a| String::from_utf8_lossy(a).into_owned()).collect()
}

/// The lowest checkpoint epoch every cell has published.
fn min_published(node: &Node) -> u64 {
    let board = node.control.ckpt_board();
    (0..board.cell_count())
        .map(|cell| board.slot(u16::try_from(cell).unwrap_or(u16::MAX)).published())
        .min()
        .unwrap_or(0)
}

/// Whether `catalog` names namespace `name`.
fn names(catalog: &NsCatalog, name: &[u8]) -> bool {
    catalog.entries.iter().any(|e| e.name == name)
}

/// The tombstone count of the planted catalog: the cap.
const PLANTED_TOMBSTONES: u32 = 256;

/// Phase 1, the stale fold (ADR-0189 D6, N14): a `DROP` at the cap with
/// every tombstone ready issues no pacing checkpoint.
fn stale_fold(
    world: &mut World,
    node: &mut Node,
    report: &mut NsDropWindowReport,
) -> Result<(), String> {
    let stamp = node.control.ckpt_last_issued();
    world.step_until(node, report, "the boot's re-stamp on every cell", |n| {
        min_published(n) >= stamp
    })?;
    let held = world.meta()?.dropped.len();
    if held != PLANTED_TOMBSTONES as usize {
        return Err(format!("VACUOUS: the stale fold began at {held} tombstones, not the cap"));
    }
    let issued = node.control.ckpt_last_issued();
    let mut client = MiniClient::connect(node, 0);
    world.expect(node, report, &mut client, &[b"INF.NS", b"DROP", b"victim"], b"+OK\r\n")?;
    report.stale_fold_epochs = node.control.ckpt_last_issued() - issued;
    if report.stale_fold_epochs != 1 {
        let seed = world.seed;
        let epochs = report.stale_fold_epochs;
        report.violations.push(format!(
            "STALE-FOLD seed {seed:#x}: the DROP at the cap issued {epochs} checkpoint epochs \
             with every tombstone ready: it paced"
        ));
    }
    Ok(())
}

/// Phase 2, the lagging arm (ADR-0189 D3 step 5, N5): a lagging export
/// inside a memory `DROP`'s window and inside a memory `CREATE` rollback's
/// window. `META` is decoded after every step of each window.
fn lagging_arm(
    world: &mut World,
    node: &mut Node,
    report: &mut NsDropWindowReport,
) -> Result<(), String> {
    let mut client = MiniClient::connect(node, 0);
    let create_lag: &[&[u8]] = &[b"INF.NS", b"CREATE", b"lag", b"MODE", b"memory"];
    world.expect(node, report, &mut client, create_lag, b"+OK\r\n")?;
    // The DROP's request queued behind the held writer, then a peer's
    // export that still names the namespace.
    node.hold_inbox = true;
    let sent = node.control.persist_requests_sent();
    client.send(node, &[b"INF.NS", b"DROP", b"lag"]);
    world.step_until(node, report, "the DROP's request", |n| {
        n.control.persist_requests_sent() > sent
    })?;
    lagging_export(node, 1, report);
    node.hold_inbox = false;
    let window = ("LAGGING-EXPORT-DROP", "a memory DROP");
    watch_window(world, node, report, &mut client, b"lag", window)?;
    // A memory CREATE whose peer leg is refused: its rollback's request,
    // then a lagging export from the origin, which still serves it.
    inf_foundation::fault::arm(NS_CREATE_FAN_REFUSED, FaultSpec::Nth(1));
    node.hold_inbox = true;
    let sent = node.control.persist_requests_sent();
    client.send(node, &[b"INF.NS", b"CREATE", b"lag2", b"MODE", b"memory"]);
    world.step_until(node, report, "the CREATE's request", |n| {
        n.control.persist_requests_sent() > sent
    })?;
    node.drain_inbox_requests(1).map_err(|e| format!("the CREATE's answer: {e}"))?;
    world.step_until(node, report, "the rollback's request", |n| {
        n.control.persist_requests_sent() > sent + 1
    })?;
    lagging_export(node, 0, report);
    node.hold_inbox = false;
    let window = ("LAGGING-EXPORT-ROLLBACK", "a rollback");
    let rolled_back = watch_window(world, node, report, &mut client, b"lag2", window);
    inf_foundation::fault::disarm(NS_CREATE_FAN_REFUSED);
    rolled_back?;
    let stale = node.control.ddl_notices_stale();
    if stale != 0 {
        let seed = world.seed;
        report.violations.push(format!(
            "NOTICE-STALE seed {seed:#x}: ns_ddl_notices_stale is {stale}: a notice retired \
             another DDL"
        ));
    }
    Ok(())
}

/// Queues cell `cell`'s export as a request with no change, the shape of
/// cell 0's readiness persist sent while a DDL waits.
fn lagging_export(node: &mut Node, cell: usize, report: &mut NsDropWindowReport) {
    let control = std::sync::Arc::clone(&node.control);
    let export = node.plane(cell).keyspace().export_catalog(
        control.next_ns_id(),
        control.next_index_id(),
        control.next_index_generation(),
    );
    control.request_persist(export);
    report.lagging_exports += 1;
}

/// Steps until `client`'s reply arrives, decoding `META` after every step:
/// it never names `name` once the DDL's request was answered.
fn watch_window(
    world: &mut World,
    node: &mut Node,
    report: &mut NsDropWindowReport,
    client: &mut MiniClient,
    name: &[u8],
    (tag, what): (&str, &str),
) -> Result<(), String> {
    let seed = world.seed;
    for _ in 0..STALL_STEPS {
        world.step(node, report)?;
        if names(&world.meta()?, name) {
            let name = String::from_utf8_lossy(name);
            report.violations.push(format!(
                "{tag} seed {seed:#x}: META names '{name}' inside {what}'s window: a lagging \
                 export brought it back"
            ));
            return Ok(());
        }
        if client.recv(node).is_some() {
            report.commands_done += 1;
            return Ok(());
        }
    }
    report.stalled = true;
    Err(format!("{what} stalled"))
}

/// The durable namespace the cut classes drop, and its keys a cell.
const LEDGER: &[u8] = b"ledger";
const KEYS_A_CELL: usize = 8;

/// Phase 3 setup: `ledger`, durable `FSYNC always`, with `KEYS_A_CELL`
/// acknowledged keys written through each cell; returns the keys.
fn seed_ledger(
    world: &mut World,
    node: &mut Node,
    report: &mut NsDropWindowReport,
) -> Result<Vec<Vec<u8>>, String> {
    let mut client = MiniClient::connect(node, 0);
    let create: &[&[u8]] =
        &[b"INF.NS", b"CREATE", LEDGER, b"MODE", b"durable", b"FSYNC", b"always"];
    world.expect(node, report, &mut client, create, b"+OK\r\n")?;
    let mut keys = Vec::new();
    for cell in 0..2 {
        let mut writer = MiniClient::connect(node, cell);
        world.expect(node, report, &mut writer, &[b"INF.NS", b"USE", LEDGER], b"+OK\r\n")?;
        for i in 0..KEYS_A_CELL {
            let key = format!("drop:cell{cell}:{i}").into_bytes();
            world.expect(node, report, &mut writer, &[b"SET", &key, b"acked"], b"+OK\r\n")?;
            keys.push(key);
        }
    }
    Ok(keys)
}

/// Phase 3: the seed's cut class, up to the cut (the caller cuts).
fn drop_at_cut(
    world: &mut World,
    node: &mut Node,
    report: &mut NsDropWindowReport,
) -> Result<(), String> {
    let drop_ledger: &[&[u8]] = &[b"INF.NS", b"DROP", LEDGER];
    let mut client = MiniClient::connect(node, 0);
    match report.cut {
        DropCut::BeforeAnswer => {
            node.hold_inbox = true;
            let sent = node.control.persist_requests_sent();
            client.send(node, drop_ledger);
            report.held_steps += world.step_until(node, report, "the DROP's request", |n| {
                n.control.persist_requests_sent() > sent
            })?;
            // Every cell publishes a checkpoint inside the wait.
            let epoch = node.ckpt_host.request(CkptTarget::All).map_err(|_| "no unit left")?;
            let epoch = epoch.get();
            report.held_steps +=
                world.step_until(node, report, "a checkpoint in the wait", |n| {
                    min_published(n) >= epoch
                })?;
            report.checkpoint_in_wait = true;
            if client.recv(node).is_some() {
                return Err("the DROP was answered while its writer was held".to_owned());
            }
        }
        DropCut::AfterAnswer | DropCut::FaultReturn => {
            inf_foundation::fault::arm(NS_DROP_AFTER_META, FaultSpec::Nth(1));
            let want = b"-ERR fault: ns_drop_after_meta\r\n";
            let dropped = world.expect(node, report, &mut client, drop_ledger, want);
            inf_foundation::fault::disarm(NS_DROP_AFTER_META);
            dropped?;
            report.held_steps += 1;
            if report.cut == DropCut::FaultReturn {
                // The node runs on: the notice was posted, so the next DDL answers.
                let next: &[&[u8]] = &[b"INF.NS", b"CREATE", b"next", b"MODE", b"memory"];
                world.expect(node, report, &mut client, next, b"+OK\r\n")?;
            }
        }
        DropCut::AfterApply => {
            inf_foundation::fault::arm(NS_DROP_AFTER_APPLY, FaultSpec::Nth(1));
            let want = b"-ERR fault: ns_drop_after_apply\r\n";
            let dropped = world.expect(node, report, &mut client, drop_ledger, want);
            inf_foundation::fault::disarm(NS_DROP_AFTER_APPLY);
            dropped?;
            report.held_steps += 1;
        }
        DropCut::AfterFan => {
            // The writer answers the drop, then is held: the fan runs and
            // the reply arrives, but the drop's notice never lands.
            let word = node.control.persisted_epoch();
            let sent = node.control.persist_requests_sent();
            client.send(node, drop_ledger);
            world.step_until(node, report, "the DROP's answer", |n| {
                n.control.persist_requests_sent() > sent && n.control.persisted_epoch() > word
            })?;
            node.hold_inbox = true;
            let mut steps = 0u64;
            let reply = loop {
                if let Some(reply) = client.recv(node) {
                    break reply;
                }
                world.step(node, report)?;
                steps += 1;
                if steps > STALL_STEPS {
                    report.stalled = true;
                    return Err("the DROP's reply stalled".to_owned());
                }
            };
            report.held_steps += steps;
            if reply != b"+OK\r\n" {
                return Err(format!("the DROP answered {:?}", String::from_utf8_lossy(&reply)));
            }
        }
    }
    Ok(())
}

/// Phase 4: after the reboot, `ledger` is whole on every cell (every
/// acknowledged key served) or gone from every cell, as the class says.
fn audit_ledger(
    world: &mut World,
    node: &mut Node,
    report: &mut NsDropWindowReport,
    keys: &[Vec<u8>],
) -> Result<(), String> {
    let seed = world.seed;
    let keeps = report.cut == DropCut::BeforeAnswer;
    for cell in 0..2 {
        let mut reader = MiniClient::connect(node, cell);
        let used = world.call(node, report, &mut reader, &[b"INF.NS", b"USE", LEDGER])?;
        if !keeps {
            if used == b"+OK\r\n" {
                report.violations.push(format!(
                    "seed {seed:#x} ({:?}): cell {cell} serves 'ledger' after a cut past the \
                     DROP's answer",
                    report.cut
                ));
            }
            continue;
        }
        if used != b"+OK\r\n" {
            report.violations.push(format!(
                "seed {seed:#x}: cell {cell} lost 'ledger' after a cut before the DROP's answer"
            ));
            continue;
        }
        for key in keys {
            let got = world.call(node, report, &mut reader, &[b"GET", key])?;
            if got == b"$5\r\nacked\r\n" {
                report.keys_served += 1;
            } else {
                let key = String::from_utf8_lossy(key);
                report.violations.push(format!(
                    "DROP-WINDOW-LOSS seed {seed:#x}: acked key {key} lost after a cut before \
                     the DROP's answer (cell {cell} read {:?})",
                    String::from_utf8_lossy(&got)
                ));
            }
        }
    }
    Ok(())
}

/// The `META` the first boot reads: 256 tombstones and a durable `victim`.
fn planted_catalog() -> NsCatalog {
    let first = inf_store::FIRST_NAMED_NS_ID;
    let victim = first + PLANTED_TOMBSTONES;
    NsCatalog {
        next_id: victim + 1,
        entries: vec![durable_spec(victim, b"victim")],
        index: IndexCatalog::default(),
        dropped: (first..victim).collect(),
    }
}

/// The phases, in order; an `Err` is the run's last violation.
fn run_phases(
    world: &mut World,
    scenario: &DurableScenario,
    observer: &TraceObserver,
    report: &mut NsDropWindowReport,
) -> Result<(), String> {
    world.disk.create_dir_all(&data_dir()).map_err(|e| format!("the data directory: {e}"))?;
    inf_log::meta::write_meta(&world.disk, &data_dir(), &planted_catalog().encode())
        .map_err(|e| format!("the planted META: {e}"))?;
    let mut node = world.boot_ready(scenario, observer, report)?;
    stale_fold(world, &mut node, report)?;
    lagging_arm(world, &mut node, report)?;
    let keys = seed_ledger(world, &mut node, report)?;
    drop_at_cut(world, &mut node, report)?;
    drop(node);
    observer.power_cut(&world.disk, world.clock.now(), world.seed ^ 0xD0A0_0FF5);
    let mut node = world.boot_ready(scenario, observer, report)?;
    audit_ledger(world, &mut node, report, &keys)?;
    let stale = node.control.ddl_notices_stale();
    if stale != 0 {
        let seed = world.seed;
        report.violations.push(format!("seed {seed:#x}: {stale} stale notices after the reboot"));
    }
    drop(node);
    Ok(())
}

fn run_observed(seed: u64, observer: &TraceObserver) -> NsDropWindowReport {
    let scenario = DurableScenario::m2_durable(seed);
    let clock = Rc::new(VirtualClock::new(Nanos(1)));
    let disk = build_disk(seed, scenario.stall.as_ref());
    let mut world = World {
        seed,
        rng: SplitMix64::new(seed ^ 0x0D0A_D0A0),
        clock: Rc::clone(&clock),
        disk,
        step_ns_max: scenario.step_ns_max,
    };
    let mut report = NsDropWindowReport {
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
        cut: DropCut::of_seed(seed),
        stale_fold_epochs: 0,
        lagging_exports: 0,
        held_steps: 0,
        checkpoint_in_wait: false,
        keys_served: 0,
        skipped_unknown_ns: 0,
    };
    if let Err(err) = run_phases(&mut world, &scenario, observer, &mut report) {
        report.violations.push(format!("seed {seed:#x}: {err}"));
    }
    inf_foundation::fault::disarm_all();
    report.trace = observer.trace_bytes();
    report.trace_hash = hash64(&report.trace, 0xD0A0);
    report.sim_seconds = clock.now().0.saturating_sub(1) as f64 / 1e9;
    report
}

/// Runs one seeded `m2-ns-drop-window` scenario and seals state evidence
/// after every node has been dropped.
#[must_use]
pub fn run_ns_drop_window_scenario(seed: u64) -> NsDropWindowReport {
    let observer = TraceObserver::default();
    let mut report = run_observed(seed, &observer);
    let sealed = observer.seal();
    report.state_hash = sealed.state_hash;
    report.cap_rows_checked = sealed.census.rows_checked;
    report.cap_rows_crossed = sealed.census.rows_crossed;
    report.violations.extend(sealed.census.violations);
    report
}
