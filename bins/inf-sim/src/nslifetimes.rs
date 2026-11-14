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
//! that a blob write's extent barrier outlives a few steps. The `full` leg:
//! 1. boot; 64 tiered `CREATE`s, each flushed on cell 0 (inline writes of
//!    keys cell 0 owns until its first tier file is open), then the 65th
//!    refused at the limit while 64 `ns-<id>` directories hold open files
//!    under cell 0's shard;
//! 2. the extent cycle: a flushed namespace dropped while a blob `GET`'s
//!    extent read and a blob `SET`'s extent barrier on it are held at the
//!    device; its successor's tiered `CREATE` is refused while they are
//!    held, accepted once they are released, and flushed on cell 0;
//! 3. the pinned cycle: a flushed namespace dropped while cold reads on its
//!    tier file are held at the device and its flush round on a peer holds
//!    its barrier there, its first unlink failing; the same successor;
//! 4. the churn: `CHURN` cycles, each dropping the namespace the last one
//!    created (every `CHURN_BLOB_EVERY`th with blob writes in flight on
//!    every cell) and creating its successor, retried while refused;
//! 5. the disconnects at 63 grants: a tiered `CREATE` whose client closes
//!    once its persist request is queued with the writer held, and one
//!    whose client closes while a peer is frozen with its leg unapplied;
//!    each alone (the namespace on every cell, also after the cut) and with
//!    a peer's leg refused (on none, also after the cut);
//! 6. the peer refusal at 63 grants: rolled back on every cell, the node's
//!    `ns_drop_tombstones` one above its settled value from the rollback
//!    until the tombstone retires; the next `CREATE` is accepted once every
//!    receipt is in, its id the last `+OK`'s + 2;
//! 7. a power cut and a reboot: every live name served on every cell, the
//!    refused ones on none, a drop and a `CREATE` taking the next id.
//!
//! The `control` leg: 63 tiered `CREATE`s flushed on cell 0 and no churn,
//! so no `CREATE` is refused and no more directories hold open files than
//! namespaces live.
//!
//! Oracles, independent of the grant book, the positions and the drain:
//! `LIFETIMES`, every step, from the sim disk's own tables: under every
//! shard at most 64 (the control leg: 63) `ns-<id>` directories hold a file
//! an open description names; `CUSTODY`, every step: no driver op named a
//! closed descriptor; `IDS`: each `+OK` `CREATE`'s id (`INF.NS INFO`) is the
//! last committed + 1, a rolled-back one committing one; `REPLY`: every
//! `CREATE` answers `+OK`, the taken-name error, a tiered or a catalog
//! capacity line, never an internal error. Each engagement clause the run
//! must meet is `VACUOUS` when missed (`engagement`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use inf_foundation::fault::FaultSpec;
use inf_foundation::hash64;
use inf_foundation::limits::TIERED_NAMESPACE_LIFETIMES_PER_NODE_MAX as LIFETIMES;
use inf_foundation::rng::{Entropy, SplitMix64};
use inf_foundation::time::{Clock, Nanos, VirtualClock};
use inf_log::fs::sim::StallConfig;
use inf_server::CkptTarget;

use crate::driver::Violations;
use crate::durable::{
    DurableScenario, MiniClient, Node, STALL_STEPS, TraceObserver, boot, build_disk, is_tier_file,
    m2_stall_config,
};

const NS_CREATE_FAN_REFUSED: &str = "ns_create_fan_refused";
/// Drop-and-create cycles of the churn.
const CHURN: u32 = 1_000;
/// Every this-many churn cycles drop a namespace with blob writes in flight.
const CHURN_BLOB_EVERY: u32 = 8;
/// Bound on the retries of one tiered `CREATE` refused while a grant is
/// Retiring (each retry waits `RETRY_GAP_STEPS`).
const RETRIES_MAX: u32 = 400;
const RETRY_GAP_STEPS: u32 = 4;
/// A held cycle's tiered `CREATE` attempts while its holds last: each is
/// refused on a node whose grant waits for every owner to end.
const HELD_TRIES: u32 = 6;
/// Blob writes in flight on each cell at a churn drop.
const BLOBS_PER_CELL: usize = 4;
/// A blob value: above the 4 KiB `BLOB-THRESHOLD`.
const BLOB_BYTES: usize = 6 << 10;
/// An inline value of the flush writes: below the `BLOB-THRESHOLD`.
const INLINE_BYTES: usize = 3_000;
/// SETs a flush batch pipelines, and the batches a flush may take: 14 × 64
/// × 3,000 bytes is about 2.6 MiB, inside the 3 MiB budget's 4 MiB ring,
/// where the first 1 MiB commit page seals and flushes.
const FLUSH_BATCH: usize = 64;
const FLUSH_BATCHES_MAX: u32 = 14;
/// Steps after a flush batch for the namespace's first tier file to open.
const FLUSH_SETTLE_STEPS: u32 = 24;
/// Cold reads a pinned cycle holds on the dropped namespace's tier file.
const COLD_READERS: usize = 4;
/// Steps a frozen-peer disconnect freezes its peer, and the steps after
/// the writer's answer before the client closes (the other legs reply).
const FREEZE_STEPS: u64 = 400;
const FANNED_STEPS: u32 = 24;
/// Rounds of a settling wait (positions drained, tombstones retired).
const SETTLE_ROUNDS: u32 = 64;
const SETTLE_GAP_STEPS: u32 = 16;

const OK: &[u8] = b"+OK\r\n";
const LIMIT: &[u8] = b"-ERR tiered namespace limit reached\r\n";
const BUSY: &[u8] = b"-TRYAGAIN tiered namespace capacity in use\r\n";
const CATALOG_BUSY: &[u8] = b"-TRYAGAIN catalog capacity in use\r\n";
const FAN_REFUSED: &[u8] = b"-ERR fault: ns_create_fan_refused\r\n";
/// Every reply a `CREATE` may answer here.
const ALLOWED: [&[u8]; 6] = [
    OK,
    b"-ERR namespace already exists\r\n",
    BUSY,
    LIMIT,
    CATALOG_BUSY,
    b"-ERR catalog limit reached\r\n",
];
/// A tiered `CREATE`'s keys: a 3 MiB budget (a 4 MiB ring) whose mutable
/// region is 1‰ of it, so the first commit page seals and flushes.
const TIERED: [&[u8]; 8] = [
    b"MODE",
    b"durable",
    b"MEM-BUDGET",
    b"3mb",
    b"BLOB-THRESHOLD",
    b"4kb",
    b"MUTABLE-FRACTION",
    b"1",
];

/// Which leg a run is.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum LifetimesLeg {
    /// Every phase, at the node's limit.
    #[default]
    Full,
    /// 63 tiered namespaces flushed on one cell and no churn: no refusal,
    /// and no more directories with open files than namespaces live.
    Control,
}

impl LifetimesLeg {
    /// The legs' names, as `--lifetimes-leg` takes them.
    pub const NAMES: [&'static str; 2] = ["full", "control"];

    #[must_use]
    pub fn parse(name: &str) -> Option<LifetimesLeg> {
        match name {
            "full" => Some(LifetimesLeg::Full),
            "control" => Some(LifetimesLeg::Control),
            _ => None,
        }
    }

    /// The most `ns-<id>` directories a shard may hold files open in.
    fn bound(self) -> usize {
        match self {
            LifetimesLeg::Full => LIFETIMES,
            LifetimesLeg::Control => LIFETIMES - 1,
        }
    }
}

/// The run's verdict and coverage disclosures.
#[derive(Debug)]
pub struct NsLifetimesReport {
    pub trace: Vec<u8>,
    pub trace_hash: u64,
    pub state_hash: u64,
    pub violations: Violations,
    /// Cap census rows read at the end of every life (node sum).
    pub cap_rows_checked: u64,
    /// Census rows the crossing rule judged (their `crossings > 0`).
    pub cap_rows_crossed: u64,
    pub stalled: bool,
    pub commands_done: u64,
    pub scheduler_steps: u64,
    pub sim_seconds: f64,
    pub cells: u16,
    pub leg: LifetimesLeg,
    /// `CREATE`s answered `+OK`.
    pub creates_ok: u64,
    /// Tiered `CREATE`s refused while a grant was Retiring, and at the limit.
    pub refused_busy: u64,
    pub refused_limit: u64,
    /// The most `ns-<id>` directories holding an open file under one shard.
    pub open_dirs_max: usize,
    /// Directories holding an open file under cell 0's shard at the first
    /// refusal at the limit.
    pub dirs_at_limit: usize,
    /// `DROP`s answered while a blob `GET`'s extent read and a blob `SET`'s
    /// extent barrier on the namespace were held at the device.
    pub extent_holds_across_drop: u64,
    /// `DROP`s answered while cold reads on the namespace's tier file were
    /// held at the device (the custody oracle's engagement).
    pub cold_reads_across_drop: u64,
    /// `DROP`s answered while the namespace's flush round on a peer held
    /// its barrier at the device.
    pub rounds_held_across_drop: u64,
    /// Tier unlinks the injected failure refused.
    pub unlink_failures: u64,
    pub churn_cycles: u64,
    /// Disconnects whose client closed with its program still running.
    pub disconnects: u64,
    /// The peer refusal's leg (1-based over the peers).
    pub refused_leg: u64,
    /// `ns_drop_tombstones` before the peer refusal and right after it.
    pub tombstones_settled: u64,
    pub tombstones_after_rollback: u64,
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
    /// The last namespace id committed.
    last_committed: u32,
    /// The live namespaces, each with the keys written to it on cell 0.
    live: BTreeMap<Vec<u8>, Vec<Vec<u8>>>,
    /// Names whose `CREATE` a peer's leg refused: served on no cell.
    refused: Vec<Vec<u8>>,
    /// The next key ordinal: every key of the run is a fresh name.
    next_key: u64,
}

/// A blob extent file, by its name.
fn is_extent(path: &Path) -> bool {
    path.extension().is_some_and(|ext| ext == "iblob")
}

/// A tier file under cell 1's shard: the pinned cycle's peer.
fn is_peer_tier(path: &Path) -> bool {
    is_tier_file(path) && path.components().any(|part| part.as_os_str() == "shard-1")
}

fn create_argv(name: &[u8]) -> Vec<&[u8]> {
    let mut argv: Vec<&[u8]> = vec![b"INF.NS", b"CREATE", name];
    argv.extend_from_slice(&TIERED);
    argv
}

fn name(prefix: &str, index: u32) -> Vec<u8> {
    format!("{prefix}{index:02}").into_bytes()
}

impl Run {
    /// Records a violation, the caller's line its check, while the report
    /// holds fewer than 16.
    #[track_caller]
    fn fail(&mut self, message: String) {
        if self.report.violations.len() < 16 {
            self.report.violations.push(message);
        }
    }

    /// One scheduler step, then the lifetime and custody oracles on the
    /// disk's own tables.
    fn step(&mut self) -> Result<(), String> {
        self.report.scheduler_steps += 1;
        self.node
            .step(&mut self.rng, &self.clock, &self.disk, self.step_ns_max)
            .map_err(|e| format!("step failed: {e}"))?;
        let bound = self.report.leg.bound();
        for (shard, dirs) in self.disk.open_ns_dirs() {
            self.report.open_dirs_max = self.report.open_dirs_max.max(dirs);
            if dirs > bound {
                self.fail(format!(
                    "LIFETIMES: {dirs} ns directories hold open files under {shard}, above \
                     {bound}"
                ));
            }
        }
        let closed = self.disk.closed_fd_ops();
        if closed > 0 {
            self.fail(format!("CUSTODY: {closed} driver ops named a closed descriptor"));
        }
        Ok(())
    }

    /// Steps until `done` holds (bounded); `what` names the wait.
    fn step_until(&mut self, what: &str, done: impl Fn(&Run) -> bool) -> Result<(), String> {
        for _ in 0..STALL_STEPS {
            if done(self) {
                return Ok(());
            }
            self.step()?;
        }
        self.report.stalled = true;
        Err(format!("{what}: never reached"))
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

    /// Steps `steps` times.
    fn idle(&mut self, steps: u32) -> Result<(), String> {
        for _ in 0..steps {
            self.step()?;
        }
        Ok(())
    }

    /// `ns-<id>` directories holding an open file under `cell`'s shard.
    fn dirs_on(&self, cell: usize) -> usize {
        self.disk.open_ns_dirs().get(&format!("shard-{cell}")).copied().unwrap_or(0)
    }

    /// The syncs and the reads `cell`'s device holds now.
    fn held(&self, cell: usize) -> (usize, usize) {
        let driver = self.node.driver(cell);
        (driver.held_syncs(), driver.held_reads())
    }

    /// Lifts every device hold: held syncs and reads reach the device at
    /// each cell's next driver entry.
    fn lift_holds(&self) {
        self.disk.hold_syncs(None);
        self.disk.hold_reads(None);
    }

    /// `count` fresh keys, each owned by `cell` (the node's slot router).
    fn owned_keys(&mut self, cell: usize, prefix: &str, count: usize) -> Vec<Vec<u8>> {
        let router = inf_store::SlotRouter::new_contiguous(self.report.cells);
        let mut keys = Vec::with_capacity(count);
        while keys.len() < count {
            let key = format!("{prefix}:{}", self.next_key).into_bytes();
            self.next_key += 1;
            if router.cell_of(inf_store::SlotRouter::slot_of(&key)).as_usize() == cell {
                keys.push(key);
            }
        }
        keys
    }

    /// A client on `cell` bound to namespace `ns`.
    fn use_ns(&mut self, cell: usize, ns: &[u8]) -> Result<MiniClient, String> {
        let mut client = MiniClient::connect(&mut self.node, cell);
        let used = self.call(&mut client, &[b"INF.NS", b"USE", ns])?;
        if used != OK {
            return Err(format!("USE {} answered {used:?}", String::from_utf8_lossy(ns)));
        }
        Ok(client)
    }

    /// Pipelines one inline `SET` of each key, every one answered `+OK`.
    fn write_inline(&mut self, client: &mut MiniClient, keys: &[Vec<u8>]) -> Result<(), String> {
        let value = vec![b'v'; INLINE_BYTES];
        for key in keys {
            client.send(&mut self.node, &[b"SET", key, &value]);
        }
        for key in keys {
            let reply = self.reply(client)?;
            if reply != OK {
                let key = String::from_utf8_lossy(key);
                return Err(format!("SET {key} answered {reply:?}"));
            }
        }
        Ok(())
    }

    /// Writes inline values of keys `cell` owns to `ns` from a client on
    /// `cell`, a batch at a time, until `done` holds after a batch's settle
    /// (bounded); the keys written on cell 0 join the namespace's record.
    fn write_until(
        &mut self,
        ns: &[u8],
        cell: usize,
        what: &str,
        done: impl Fn(&Run) -> bool,
    ) -> Result<(), String> {
        let mut client = self.use_ns(cell, ns)?;
        for _ in 0..FLUSH_BATCHES_MAX {
            let keys = self.owned_keys(cell, "k", FLUSH_BATCH);
            self.write_inline(&mut client, &keys)?;
            if cell == 0
                && let Some(written) = self.live.get_mut(ns)
            {
                written.extend(keys);
            }
            for _ in 0..FLUSH_SETTLE_STEPS {
                if done(self) {
                    client.close(&mut self.node);
                    return Ok(());
                }
                self.step()?;
            }
        }
        Err(format!("VACUOUS: {} never {what}", String::from_utf8_lossy(ns)))
    }

    /// Flushes `ns` on cell 0: its writes there until one more `ns-<id>`
    /// directory under cell 0's shard holds an open file, its first tier
    /// file.
    fn flush_on_cell_0(&mut self, ns: &[u8]) -> Result<(), String> {
        let before = self.dirs_on(0);
        self.write_until(ns, 0, "flushed on cell 0", move |run| run.dirs_on(0) > before)
    }
}

impl Run {
    /// `INF.NS INFO name`'s `id`.
    fn ns_id(&mut self, client: &mut MiniClient, name: &[u8]) -> Result<u32, String> {
        let frame = self.call(client, &[b"INF.NS", b"INFO", name])?;
        let text = String::from_utf8_lossy(&frame).into_owned();
        let lines: Vec<&str> = text.split("\r\n").collect();
        let at = lines.iter().position(|line| *line == "id").ok_or(format!("no id in {text:?}"))?;
        lines.get(at + 2).and_then(|id| id.parse().ok()).ok_or(format!("no id in {text:?}"))
    }

    /// The `field` of `INFO section`, an integer.
    fn info(
        &mut self,
        client: &mut MiniClient,
        section: &[u8],
        field: &str,
    ) -> Result<u64, String> {
        let frame = self.call(client, &[b"INFO", section])?;
        let text = String::from_utf8_lossy(&frame).into_owned();
        let value = text
            .split("\r\n")
            .find_map(|line| line.strip_prefix(field)?.strip_prefix(':')?.parse().ok());
        value.ok_or(format!("no {field} in INFO {}", String::from_utf8_lossy(section)))
    }

    /// The `id` check for an accepted `CREATE` of `name`.
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
        self.live.insert(name.to_vec(), Vec::new());
        Ok(())
    }

    /// One `CREATE` and its reply, the reply judged.
    fn create_with(&mut self, client: &mut MiniClient, argv: &[&[u8]]) -> Result<Vec<u8>, String> {
        let name = argv[2];
        let reply = self.call(client, argv)?;
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

    /// One tiered `CREATE` of `name` and its reply.
    fn create(&mut self, client: &mut MiniClient, name: &[u8]) -> Result<Vec<u8>, String> {
        self.create_with(client, &create_argv(name))
    }

    /// A tiered `CREATE` of `name` retried while a grant or a catalog slot
    /// is held, until it is accepted (bounded).
    fn create_until_ok(&mut self, client: &mut MiniClient, name: &[u8]) -> Result<(), String> {
        for _ in 0..RETRIES_MAX {
            let reply = self.create(client, name)?;
            if reply == OK {
                return Ok(());
            }
            if reply == BUSY {
                self.report.refused_busy += 1;
            } else if reply != CATALOG_BUSY {
                return Err(format!(
                    "CREATE {} answered {:?} while a grant returns",
                    String::from_utf8_lossy(name),
                    String::from_utf8_lossy(&reply)
                ));
            }
            self.idle(RETRY_GAP_STEPS)?;
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
        self.live.remove(name);
        Ok(())
    }

    /// The cells that answer `+OK` to `INF.NS USE name`, one fresh client
    /// each.
    fn served_on(&mut self, name: &[u8]) -> Result<Vec<u16>, String> {
        let mut served = Vec::new();
        for cell in 0..self.report.cells {
            let mut probe = MiniClient::connect(&mut self.node, usize::from(cell));
            let reply = self.call(&mut probe, &[b"INF.NS", b"USE", name])?;
            probe.close(&mut self.node);
            if reply == OK {
                served.push(cell);
            } else if !reply.starts_with(b"-ERR namespace") {
                return Err(format!("cell {cell}: USE answered {reply:?}"));
            }
        }
        Ok(served)
    }

    /// Steps until every cell's `tier_ns_positions` reads `held`: every
    /// dropped owner drained and its receipt published (bounded).
    fn wait_positions(&mut self, held: u64) -> Result<(), String> {
        for _ in 0..SETTLE_ROUNDS {
            let mut drained = true;
            for cell in 0..usize::from(self.report.cells) {
                let mut probe = MiniClient::connect(&mut self.node, cell);
                let positions = self.info(&mut probe, b"tripwires", "tier_ns_positions")?;
                probe.close(&mut self.node);
                drained &= positions == held;
            }
            if drained {
                return Ok(());
            }
            self.idle(SETTLE_GAP_STEPS)?;
        }
        self.report.stalled = true;
        Err(format!("the cells never held {held} positions"))
    }

    /// Steps until every cell recovered (bounded).
    fn recover(&mut self) -> Result<(), String> {
        self.step_until("recovery", |run| run.node.ready())
    }
}

/// The scenario's node: the m2 durable shape on two or four cells, 64 KiB
/// segments, a checkpoint every 4 MiB of log, a device whose fsyncs take
/// 20 ms against steps of at most 1 ms.
fn scenario(seed: u64) -> DurableScenario {
    let stall = StallConfig { base_ns: 20_000_000, ..m2_stall_config() };
    DurableScenario {
        cells: if seed.is_multiple_of(2) { 2 } else { 4 },
        segment_bytes: 64 << 10,
        ckpt_interval_bytes: 4 << 20,
        step_ns_max: 1_000_000,
        stall: Some(stall),
        ..DurableScenario::m2_durable(seed)
    }
}

/// Phase 1: the leg's tiered `CREATE`s, each flushed on cell 0; on the full
/// leg the 65th is refused at the limit with 64 directories holding files.
fn fill(run: &mut Run, admin: &mut MiniClient) -> Result<(), String> {
    for index in 0..run.report.leg.bound() as u32 {
        let ns = name("t", index);
        let reply = run.create(admin, &ns)?;
        if reply != OK {
            return Err(format!("tiered CREATE {index} below the limit answered {reply:?}"));
        }
        run.flush_on_cell_0(&ns)?;
    }
    if run.report.leg == LifetimesLeg::Control {
        return Ok(());
    }
    let reply = run.create(admin, b"over")?;
    run.report.dirs_at_limit = run.dirs_on(0);
    if reply == LIMIT {
        run.report.refused_limit += 1;
    } else {
        run.fail(format!("VACUOUS: the 65th tiered CREATE answered {reply:?}, not the limit"));
    }
    Ok(())
}

/// A held cycle's successor: its tiered `CREATE` tried while the holds
/// last, each refused on a node whose grant waits for every owner of the
/// dropped namespace to end; then, the holds lifted, retried until `+OK`.
/// A node that returned the grant early accepts it at once, and the
/// successor is flushed beside the dropped owner's open files: the held
/// barriers are released first (a write's reply waits behind an earlier
/// barrier of its cell), the held reads kept.
fn successor_after_holds(
    run: &mut Run,
    admin: &mut MiniClient,
    successor: &[u8],
) -> Result<(), String> {
    for _ in 0..HELD_TRIES {
        let reply = run.create(admin, successor)?;
        if reply == OK {
            run.disk.hold_syncs(None);
            return run.flush_on_cell_0(successor);
        }
        if reply != BUSY {
            return Err(format!("a held cycle's CREATE answered {reply:?}"));
        }
        run.report.refused_busy += 1;
        run.idle(RETRY_GAP_STEPS)?;
    }
    run.lift_holds();
    run.create_until_ok(admin, successor)?;
    run.flush_on_cell_0(successor)
}

/// Phase 2, the extent cycle: `victim`, flushed on cell 0, is dropped while
/// a blob `GET`'s extent read and a blob `SET`'s extent barrier on it are
/// held at cell 0's device; then its successor (`successor_after_holds`).
fn extent_cycle(
    run: &mut Run,
    admin: &mut MiniClient,
    victim: &[u8],
    successor: &[u8],
) -> Result<(), String> {
    let value = vec![b'b'; BLOB_BYTES];
    let mut writer = run.use_ns(0, victim)?;
    let stored = run.owned_keys(0, "xb", 1).remove(0);
    let reply = run.call(&mut writer, &[b"SET", &stored, &value])?;
    if reply != OK {
        return Err(format!("the stored blob's SET answered {reply:?}"));
    }
    run.disk.hold_syncs(Some(is_extent));
    run.disk.hold_reads(Some(is_extent));
    let barrier = run.owned_keys(0, "xw", 1).remove(0);
    writer.send(&mut run.node, &[b"SET", &barrier, &value]);
    let mut reader = run.use_ns(0, victim)?;
    reader.send(&mut run.node, &[b"GET", &stored]);
    run.step_until("the extent read and barrier held", |run| {
        let (syncs, reads) = run.held(0);
        syncs > 0 && reads > 0
    })?;
    run.drop_ns(admin, victim)?;
    let (syncs, reads) = run.held(0);
    if syncs > 0 && reads > 0 {
        run.report.extent_holds_across_drop += 1;
    }
    successor_after_holds(run, admin, successor)?;
    run.lift_holds();
    let set = run.reply(&mut writer)?;
    let get = run.reply(&mut reader)?;
    if set != OK && !set.starts_with(b"-") {
        run.fail(format!("REPLY: the held barrier's SET answered {set:?}"));
    }
    if !get.starts_with(b"$") && !get.starts_with(b"-") {
        run.fail(format!("REPLY: the held extent read's GET answered {get:?}"));
    }
    Ok(())
}

/// Phase 3, the pinned cycle: `victim`, flushed on cell 0, is dropped while
/// cold reads of its flushed keys are held at cell 0's device, its flush
/// round on cell 1 holds its barrier there, and its first unlink fails;
/// then its successor (`successor_after_holds`).
fn pinned_cycle(
    run: &mut Run,
    admin: &mut MiniClient,
    victim: &[u8],
    successor: &[u8],
) -> Result<(), String> {
    run.disk.hold_syncs(Some(is_peer_tier));
    run.write_until(victim, 1, "held a flush barrier on cell 1", |run| run.held(1).0 > 0)?;
    run.disk.hold_reads(Some(is_tier_file));
    let keys: Vec<Vec<u8>> = run
        .live
        .get(victim)
        .map(|keys| keys.iter().take(COLD_READERS).cloned().collect())
        .unwrap_or_default();
    let mut readers = Vec::with_capacity(keys.len());
    for key in &keys {
        let mut reader = run.use_ns(0, victim)?;
        reader.send(&mut run.node, &[b"GET", key]);
        readers.push(reader);
    }
    run.step_until("a cold read held", |run| run.held(0).1 > 0)?;
    inf_foundation::fault::arm(inf_log::fault::TIER_UNLINK_FAIL, FaultSpec::Nth(1));
    run.drop_ns(admin, victim)?;
    if run.held(0).1 > 0 {
        run.report.cold_reads_across_drop += 1;
    }
    if run.held(1).0 > 0 {
        run.report.rounds_held_across_drop += 1;
    }
    let tried = successor_after_holds(run, admin, successor);
    run.report.unlink_failures += inf_foundation::fault::fired(inf_log::fault::TIER_UNLINK_FAIL);
    inf_foundation::fault::disarm(inf_log::fault::TIER_UNLINK_FAIL);
    tried?;
    run.lift_holds();
    for mut reader in readers {
        let got = run.reply(&mut reader)?;
        if !got.starts_with(b"$") && !got.starts_with(b"-") {
            run.fail(format!("REPLY: a held cold read's GET answered {got:?}"));
        }
    }
    Ok(())
}

/// Blob writes in flight on `ns` on every cell: their extent barriers hold
/// its owners at a drop. The writers, their replies unread.
fn blob_writes(run: &mut Run, ns: &[u8], cycle: u32) -> Result<Vec<MiniClient>, String> {
    let value = vec![b'b'; BLOB_BYTES];
    let mut writers = Vec::new();
    for cell in 0..usize::from(run.report.cells) {
        let mut writer = run.use_ns(cell, ns)?;
        for blob in 0..BLOBS_PER_CELL {
            let key = format!("blob:{cycle}:{cell}:{blob}").into_bytes();
            writer.send(&mut run.node, &[b"SET", &key, &value]);
        }
        writers.push(writer);
    }
    Ok(writers)
}

/// Phase 4: each cycle drops `victim` (every `CHURN_BLOB_EVERY`th with blob
/// writes in flight on every cell), then creates its successor, retried
/// while refused. Returns the last namespace created.
fn churn(run: &mut Run, admin: &mut MiniClient, mut victim: Vec<u8>) -> Result<Vec<u8>, String> {
    for cycle in 0..CHURN {
        let writers = if cycle.is_multiple_of(CHURN_BLOB_EVERY) {
            blob_writes(run, &victim, cycle)?
        } else {
            Vec::new()
        };
        run.drop_ns(admin, &victim)?;
        let next = format!("c{cycle:04}").into_bytes();
        run.create_until_ok(admin, &next)?;
        for mut writer in writers {
            for _ in 0..BLOBS_PER_CELL {
                run.reply(&mut writer)?;
            }
            writer.close(&mut run.node);
        }
        victim = next;
        run.report.churn_cycles += 1;
    }
    Ok(victim)
}

/// How a disconnect's `CREATE` is caught when its client closes.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Caught {
    /// Its persist request queued, the writer held.
    Inbox,
    /// The writer answered and the legs went out, the last peer frozen
    /// with its leg unapplied.
    FrozenPeer,
}

/// One disconnect at 63 grants: a tiered `CREATE` of `name` from cell 0
/// whose client closes once it is `caught`, the first peer's leg refused
/// when `refused`. The program runs to its end either way: alone, the
/// namespace on every cell and its id the last committed + 1; refused, on
/// none, its id committed. Either way the ticket is released.
fn disconnect(
    run: &mut Run,
    admin: &mut MiniClient,
    name: &[u8],
    caught: Caught,
    refused: bool,
) -> Result<(), String> {
    if refused {
        inf_foundation::fault::arm(NS_CREATE_FAN_REFUSED, FaultSpec::Nth(1));
    }
    let mut client = MiniClient::connect(&mut run.node, 0);
    let sent = run.node.control.persist_requests_sent();
    run.node.hold_inbox = caught == Caught::Inbox;
    client.send(&mut run.node, &create_argv(name));
    run.step_until("the disconnected CREATE's request", |run| {
        run.node.control.persist_requests_sent() > sent
    })?;
    if caught == Caught::FrozenPeer {
        run.node.frozen = Some((usize::from(run.report.cells - 1), FREEZE_STEPS));
        run.step_until("the writer's answer", |run| run.node.control.ddl_pending() == 1)?;
        run.idle(FANNED_STEPS)?;
    }
    let frozen = run.node.frozen.is_some_and(|(_, left)| left > 0);
    let open = run.node.control.ddl_holder().is_some() && (caught == Caught::Inbox || frozen);
    client.close(&mut run.node);
    run.node.hold_inbox = false;
    if open {
        run.report.disconnects += 1;
    } else {
        run.fail(format!("VACUOUS: the {caught:?} disconnect closed after its program ended"));
    }
    run.step_until("the disconnected CREATE's end", |run| {
        run.node.control.ddl_holder().is_none() && run.node.frozen.is_none()
    })?;
    let served = run.served_on(name)?;
    if refused {
        inf_foundation::fault::disarm(NS_CREATE_FAN_REFUSED);
        run.last_committed += 1;
        run.refused.push(name.to_vec());
        if !served.is_empty() {
            run.fail(format!("a refused {caught:?} disconnect is served on cells {served:?}"));
        }
        return Ok(());
    }
    run.accepted(admin, name)?;
    if served.len() != usize::from(run.report.cells) {
        run.fail(format!("a {caught:?} disconnect is served on cells {served:?} only"));
    }
    Ok(())
}

/// Phase 5: the four disconnects at 63 grants. After each, the node is back
/// at 63 with every receipt in: an accepted one drops the lowest `t`
/// namespace; a refused one's grant returns through a probe's `CREATE`.
fn disconnects(run: &mut Run, admin: &mut MiniClient) -> Result<(), String> {
    let forms: [(&[u8], Caught, bool); 4] = [
        (b"d00", Caught::Inbox, false),
        (b"d01", Caught::FrozenPeer, false),
        (b"e00", Caught::Inbox, true),
        (b"e01", Caught::FrozenPeer, true),
    ];
    for (index, (form, caught, refused)) in forms.into_iter().enumerate() {
        disconnect(run, admin, form, caught, refused)?;
        let drop = if refused {
            let probe = name("s", index as u32);
            run.create_until_ok(admin, &probe)?;
            probe
        } else {
            let lowest = run.live.keys().find(|ns| ns.starts_with(b"t")).cloned();
            lowest.ok_or("no t namespace left to drop")?
        };
        run.drop_ns(admin, &drop)?;
        run.wait_positions(LIFETIMES as u64 - 1)?;
    }
    Ok(())
}

/// `ns_drop_tombstones`, the node's live drop tombstones.
fn tombstones(run: &mut Run, admin: &mut MiniClient) -> Result<u64, String> {
    run.info(admin, b"persistence", "ns_drop_tombstones")
}

/// Every cell publishes a fresh checkpoint, then the writer answers an
/// accepted request (a memory namespace's `CREATE` and `DROP`), which
/// retires each tombstone the checkpoint covers.
fn retire_round(run: &mut Run, admin: &mut MiniClient, index: u32) -> Result<(), String> {
    let epoch = run.node.ckpt_host.request(CkptTarget::All).map_err(|_| "no unit left")?;
    let epoch = epoch.get();
    run.step_until("a checkpoint every cell published", |run| {
        let board = run.node.control.ckpt_board();
        (0..board.cell_count())
            .all(|cell| board.slot(u16::try_from(cell).unwrap_or(u16::MAX)).published() >= epoch)
    })?;
    let memory = name("m", index);
    let argv: [&[u8]; 5] = [b"INF.NS", b"CREATE", &memory, b"MODE", b"memory"];
    let reply = run.create_with(admin, &argv)?;
    if reply != OK {
        return Err(format!("a memory CREATE answered {reply:?}"));
    }
    run.drop_ns(admin, &memory)
}

/// Retire rounds until `ns_drop_tombstones` reads `want`, never reading
/// above `ceiling` (bounded). Returns the rounds taken.
fn tombstones_to(
    run: &mut Run,
    admin: &mut MiniClient,
    want: u64,
    ceiling: u64,
    first: u32,
) -> Result<u32, String> {
    for round in 0..SETTLE_ROUNDS {
        let live = tombstones(run, admin)?;
        if live > ceiling {
            run.fail(format!("ns_drop_tombstones read {live}, above {ceiling}"));
        }
        if live == want {
            return Ok(round);
        }
        retire_round(run, admin, first + round)?;
    }
    run.report.stalled = true;
    Err(format!("ns_drop_tombstones never read {want}"))
}

/// Phase 6: the peer refusal at 63 grants, with the tombstone count settled
/// first.
fn peer_refusal(run: &mut Run, admin: &mut MiniClient) -> Result<(), String> {
    let rounds = tombstones_to(run, admin, 0, u64::MAX, 0)?;
    run.report.tombstones_settled = tombstones(run, admin)?;
    let refused_leg = 1 + run.rng.next_below(u64::from(run.report.cells) - 1);
    run.report.refused_leg = refused_leg;
    inf_foundation::fault::arm(NS_CREATE_FAN_REFUSED, FaultSpec::Nth(refused_leg));
    let reply = run.call(admin, &create_argv(b"r00"))?;
    inf_foundation::fault::disarm(NS_CREATE_FAN_REFUSED);
    if reply != FAN_REFUSED {
        return Err(format!("VACUOUS: the refused leg's CREATE answered {reply:?}"));
    }
    // The writer committed its id at the acceptance the rollback followed.
    run.last_committed += 1;
    run.refused.push(b"r00".to_vec());
    let after = tombstones(run, admin)?;
    run.report.tombstones_after_rollback = after;
    let settled = run.report.tombstones_settled;
    if after != settled + 1 {
        run.fail(format!("ns_drop_tombstones read {after} after the rollback, not {settled} + 1"));
    }
    let served = run.served_on(b"r00")?;
    if !served.is_empty() {
        run.fail(format!("a CREATE refused at a peer's leg is served on cells {served:?}"));
    }
    run.create_until_ok(admin, b"p00")?;
    tombstones_to(run, admin, settled, settled + 1, rounds + 1).map(|_| ())
}

/// Runs one seeded `m4-ns-lifetimes` leg.
fn run_observed(seed: u64, leg: LifetimesLeg, observer: TraceObserver) -> NsLifetimesReport {
    let scenario = scenario(seed);
    let clock = Rc::new(VirtualClock::new(Nanos(1)));
    let disk = build_disk(seed, scenario.stall.as_ref());
    let report = empty_report(scenario.cells, leg);
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
        live: BTreeMap::new(),
        refused: Vec::new(),
        next_key: 0,
    };
    if let Err(err) = phases(&mut run) {
        run.fail(err);
        return finish(run.report, &observer, &clock);
    }
    if leg == LifetimesLeg::Control {
        control_checks(&mut run);
        return finish(run.report, &observer, &clock);
    }
    reboot(run, &scenario, seed, &observer, &clock)
}

/// The cut and the reboot: the old node is gone before the disk is cut;
/// then phase 7 and the engagement verdict.
fn reboot(
    run: Run,
    scenario: &DurableScenario,
    seed: u64,
    observer: &TraceObserver,
    clock: &Rc<VirtualClock>,
) -> NsLifetimesReport {
    let Run {
        node,
        rng,
        clock: run_clock,
        disk,
        step_ns_max,
        report,
        last_committed,
        live,
        refused,
        next_key,
    } = run;
    drop(node);
    observer.power_cut(&disk, run_clock.now(), seed ^ 0x11FE_0C07);
    let node = match boot(scenario, PathBuf::from("node"), &disk, &run_clock, observer) {
        Ok(node) => node,
        Err(err) => {
            let mut report = report;
            report.violations.push(format!("reboot after the cut refused: {err}"));
            return finish(report, observer, clock);
        }
    };
    let mut run = Run {
        node,
        rng,
        clock: run_clock,
        disk,
        step_ns_max,
        report,
        last_committed,
        live,
        refused,
        next_key,
    };
    if let Err(err) = after_cut(&mut run) {
        run.fail(err);
    }
    engagement(&mut run);
    finish(run.report, observer, clock)
}

fn empty_report(cells: u16, leg: LifetimesLeg) -> NsLifetimesReport {
    NsLifetimesReport {
        trace: Vec::new(),
        trace_hash: 0,
        state_hash: 0,
        violations: Violations::default(),
        cap_rows_checked: 0,
        cap_rows_crossed: 0,
        stalled: false,
        commands_done: 0,
        scheduler_steps: 0,
        sim_seconds: 0.0,
        cells,
        leg,
        creates_ok: 0,
        refused_busy: 0,
        refused_limit: 0,
        open_dirs_max: 0,
        dirs_at_limit: 0,
        extent_holds_across_drop: 0,
        cold_reads_across_drop: 0,
        rounds_held_across_drop: 0,
        unlink_failures: 0,
        churn_cycles: 0,
        disconnects: 0,
        refused_leg: 0,
        tombstones_settled: 0,
        tombstones_after_rollback: 0,
    }
}

/// Phases 1–6 on one life (the control leg: phase 1 alone).
fn phases(run: &mut Run) -> Result<(), String> {
    run.recover()?;
    let mut admin = MiniClient::connect(&mut run.node, 0);
    fill(run, &mut admin)?;
    if run.report.leg == LifetimesLeg::Control {
        return run.idle(256);
    }
    extent_cycle(run, &mut admin, b"t00", b"n00")?;
    pinned_cycle(run, &mut admin, b"t01", b"n01")?;
    let last = churn(run, &mut admin, b"n01".to_vec())?;
    run.drop_ns(&mut admin, &last)?;
    run.wait_positions(LIFETIMES as u64 - 1)?;
    disconnects(run, &mut admin)?;
    peer_refusal(run, &mut admin)
}

/// The control leg's verdict: no refusal, and the directories holding open
/// files under cell 0's shard are its 63 namespaces.
fn control_checks(run: &mut Run) {
    if run.report.refused_busy + run.report.refused_limit > 0 {
        run.fail(format!(
            "CONTROL: {} tiered CREATEs refused below the limit",
            run.report.refused_busy + run.report.refused_limit
        ));
    }
    let dirs = run.dirs_on(0);
    if dirs != LIFETIMES - 1 {
        run.fail(format!("VACUOUS: {dirs} directories hold open files under shard-0, not 63"));
    }
}

/// Phase 7, after the cut: every live name on every cell, the refused ones
/// on none, and a `CREATE` after a drop taking the next id.
fn after_cut(run: &mut Run) -> Result<(), String> {
    run.recover()?;
    let live: Vec<Vec<u8>> = run.live.keys().cloned().collect();
    for name in &live {
        let served = run.served_on(name)?;
        if served.len() != usize::from(run.report.cells) {
            run.fail(format!(
                "after the cut {} is served on cells {served:?} only",
                String::from_utf8_lossy(name)
            ));
        }
    }
    for name in run.refused.clone() {
        let served = run.served_on(&name)?;
        if !served.is_empty() {
            run.fail(format!(
                "after the cut the refused {} is served on cells {served:?}",
                String::from_utf8_lossy(&name)
            ));
        }
    }
    let mut admin = MiniClient::connect(&mut run.node, 0);
    run.drop_ns(&mut admin, b"p00")?;
    run.create_until_ok(&mut admin, b"z00")
}

/// The full leg's engagement: each clause unmet is `VACUOUS`.
fn engagement(run: &mut Run) {
    let r = &run.report;
    let clauses = [
        (r.dirs_at_limit == LIFETIMES, "64 directories held open files at the limit's refusal"),
        (r.refused_limit > 0, "a tiered CREATE was refused at the limit"),
        (r.refused_busy > 0, "a tiered CREATE met a Retiring grant"),
        (r.extent_holds_across_drop > 0, "an extent read and barrier were held across a DROP"),
        (r.cold_reads_across_drop > 0, "cold reads were held across a DROP"),
        (r.rounds_held_across_drop > 0, "a peer's flush round was held across a DROP"),
        (r.unlink_failures > 0, "a drained tier file's unlink failed"),
        (r.churn_cycles == u64::from(CHURN), "the churn ran every cycle"),
        (r.disconnects == 4, "every disconnect closed with its program running"),
    ];
    let missed: Vec<&str> = clauses.iter().filter(|(met, _)| !met).map(|(_, what)| *what).collect();
    for what in missed {
        run.fail(format!("VACUOUS: not met: {what}"));
    }
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

/// Runs one leg of the scenario and seals state evidence after every node
/// has been dropped.
#[must_use]
pub fn run_ns_lifetimes_scenario(seed: u64, leg: LifetimesLeg) -> NsLifetimesReport {
    let observer = TraceObserver::default();
    let mut report = run_observed(seed, leg, observer.clone());
    let sealed = observer.seal();
    report.state_hash = sealed.state_hash;
    report.cap_rows_checked = sealed.census.rows_checked;
    report.cap_rows_crossed = sealed.census.rows_crossed;
    report.violations.extend(sealed.census.violations);
    report
}
