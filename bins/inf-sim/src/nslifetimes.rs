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
//! 4. the held-worker cycle: a flushed namespace dropped with the control
//!    thread's retirement worker held, its tier files out as jobs; its
//!    successor refused while they are out, and again while the worker
//!    runs with every unlink failing; accepted once they succeed;
//! 5. the churn: `CHURN` cycles, each dropping the namespace the last one
//!    created (every `CHURN_BLOB_EVERY`th with blob writes in flight on
//!    every cell) and creating its successor, retried while refused;
//! 6. the disconnects at 63 grants: a tiered `CREATE` whose client closes
//!    once its persist request is queued with the writer held, and one
//!    whose client closes while a peer is frozen with its leg unapplied;
//!    each alone (the namespace on every cell, also after the cut) and with
//!    a peer's leg refused (on none, also after the cut);
//! 7. the peer refusal at 63 grants: rolled back on every cell, the node's
//!    `ns_drop_tombstones` one above its settled value from the rollback
//!    until the tombstone retires; the next `CREATE` is accepted once every
//!    receipt is in, its id the last `+OK`'s + 2;
//! 8. a power cut and a reboot: every live name served on every cell, the
//!    refused ones on none, a drop and a `CREATE` taking the next id.
//!
//! Between the held-worker cycle and the churn, the full pass: eight
//! namespaces, each with a tier file on cell 0 and one on cell 1, dropped
//! with the worker held, so two cells each hold their 8 job positions out;
//! the worker then runs, and its pass unlinks 8 of the 16 ready; their
//! successors restore the node's 64.
//!
//! The `control` leg: 63 tiered `CREATE`s flushed on cell 0 and no churn,
//! so no `CREATE` is refused and no more directories hold open files than
//! namespaces live.
//!
//! The `pace` leg (O6's, steps of at most 201 µs): one namespace's one tier
//! file dropped with `tier_close_fail` and `tier_unlink_fail` armed once, so
//! its close fails, then its retry's unlink, then the next unlink removes
//! it. The `create-fault` leg (O7's): a tiered namespace's first create
//! refused by `tier_create_fail` once, another's by `tier_create_post_fail`
//! once, each residue's unlink held across a MAINTAIN; a third's failing on
//! every attempt `CREATE_EVERY_ATTEMPTS` times; a fourth dropped while its
//! residue is out.
//!
//! Oracles, independent of the grant book, the positions and the drain:
//! `LIFETIMES`, every step, from the sim disk's own tables: under every
//! shard at most 64 (the control leg: 63) `ns-<id>` directories hold a file
//! an open description names; `CUSTODY`, every step: no driver op named a
//! closed descriptor; `IDS`: each `+OK` `CREATE`'s id (`INF.NS INFO`) is the
//! last committed + 1, a rolled-back one committing one; `REPLY`: every
//! `CREATE` answers `+OK`, the taken-name error, a tiered or a catalog
//! capacity line, never an internal error; `HELD-WORKER`: the held-worker
//! cycle's successor answers `+OK` only once no tier file of the dropped
//! namespace is left on the disk; `RETIRE-STEP` (O3's per-step clause, every
//! leg, every step after recovery): every tier-file unlink is the worker's
//! pass's and a step makes at most 8; `RETIRE-WINDOW` (O3's window, the
//! full leg): once every dropped owner has drained, no tier file of a
//! namespace dropped in the life is on the disk; `RETRY-PACE` (O6, every
//! leg): a failed close's or unlink's retry lies at least 1 ms after it;
//! `FAILED-CREATE` (O7, the create-fault leg). Each engagement clause the
//! run must meet is `VACUOUS` when missed (`engagement`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use inf_foundation::fault::FaultSpec;
use inf_foundation::hash64;
use inf_foundation::limits::TIERED_NAMESPACE_LIFETIMES_PER_NODE_MAX as LIFETIMES;
use inf_foundation::rng::{Entropy, SplitMix64};
use inf_foundation::time::{Clock, Nanos, VirtualClock};
use inf_log::fs::sim::{CallResult, DiskCall, StallConfig};
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
/// Steps the held-worker cycle runs its worker with every unlink failing:
/// each a millisecond at most, so each file is tried again many times.
const EVERY_ATTEMPT_STEPS: u32 = 32;
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
/// The full pass's namespaces: 8 a cell fill its 8 job positions.
const FULL_PASS_VICTIMS: u32 = 8;
/// The `pace` leg's largest step: O6's pace leg is judged at steps of at
/// most 201 µs, so a retry the walk offers early lands within a step or two.
const PACE_STEP_NS_MAX: u64 = 200_000;
/// Failed creates the create-fault leg's every-attempt namespace meets
/// (the record's 10⁴, each a round trip of two steps).
const CREATE_EVERY_ATTEMPTS: u64 = 10_000;
/// Steps a held residue's unlink is held after its failed create: more than
/// one MAINTAIN, so a create runs while the residue's job is out.
const RESIDUE_HELD_STEPS: u32 = 4;
/// Steps the every-attempt namespace may take for its failures: two a
/// failure, with room for the node's other work.
const CREATE_EVERY_ATTEMPT_STEPS: u64 = 4 * CREATE_EVERY_ATTEMPTS;
/// Injected time a window's opening may wait for the node's quiescence: the
/// checkpoint the drops begin syncs its begin marker at an everysec tick and
/// its MANIFEST swap waits for the durable watermark at the next, so a quiet
/// node is at most about three everysec periods away (2,500 to 4,700 steps
/// were read across seeds, 1.5 to 2.5 s); past four periods the opening is a
/// breach, never a skip.
const WINDOW_OPEN_NS_MAX: u64 = 4_000_000_000;
/// Steps a window may run at most, its files' bound and the extension for
/// files created inside it together: no leg creates more than a few files
/// inside a window, so this ends a runaway extension, never a correct one.
const WINDOW_STEPS_MAX: u64 = 1_024;
/// Checkpoints the life after the cut may request before its dead file is
/// retired by a landed swap (the first stamps the deaths).
const R0_CKPTS_MAX: u32 = 8;
/// The life after the cut's dropped namespaces: 8 files and R0's one on one
/// shard is a bill of 9 files in 8 steps, which one job a pass misses.
const REBOOT_VICTIMS: usize = 8;
/// Steps the graceful stop may take: the stop's final sync and checkpoint
/// are a few 20 ms barriers, then each cell's transport retires in four
/// steps.
const STOP_STEPS_MAX: u32 = 8_192;

/// Write batches the disk leg may take to seal its first tier file: the
/// 4 MiB ring wraps after about 22 batches of 64 × 3,000 B.
const DISK_FILL_BATCHES_MAX: u32 = 64;
/// Checkpoints the disk leg may request before its dead file retires: the
/// first stamps the deaths, a later one's swap retires the file.
const DISK_CKPTS_MAX: u32 = 8;
/// Writes the disk leg tries at the clamped budget, each refused, and after
/// the release, until one is admitted.
const DISK_WRITES_MAX: u32 = 32;

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
    /// O6's pace leg: one retiring file's close and its retry's unlink fail
    /// once each, on steps of at most 201 µs.
    Pace,
    /// O7's leg: failed creates, before and after their file exists, once,
    /// on every attempt and under a drop.
    CreateFault,
    /// The disk row: a live namespace's retired file failing its unlink
    /// on every attempt keeps its bytes counted against the disk budget.
    Disk,
}

impl LifetimesLeg {
    /// The legs' names, as `--lifetimes-leg` takes them.
    pub const NAMES: [&'static str; 5] = ["full", "control", "pace", "create-fault", "disk"];

    #[must_use]
    pub fn parse(name: &str) -> Option<LifetimesLeg> {
        match name {
            "full" => Some(LifetimesLeg::Full),
            "control" => Some(LifetimesLeg::Control),
            "pace" => Some(LifetimesLeg::Pace),
            "create-fault" => Some(LifetimesLeg::CreateFault),
            "disk" => Some(LifetimesLeg::Disk),
            _ => None,
        }
    }

    /// The most `ns-<id>` directories a shard may hold files open in.
    fn bound(self) -> usize {
        match self {
            LifetimesLeg::Full
            | LifetimesLeg::Pace
            | LifetimesLeg::CreateFault
            | LifetimesLeg::Disk => LIFETIMES,
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
    /// `DROP`s answered with the retirement worker held and a tier file of
    /// the namespace out as a job.
    pub jobs_held_across_drop: u64,
    /// Unlinks refused while every attempt failed (the held-worker cycle).
    pub unlink_every_attempt: u64,
    pub churn_cycles: u64,
    /// Disconnects whose client closed with its program still running.
    pub disconnects: u64,
    /// The peer refusal's leg (1-based over the peers).
    pub refused_leg: u64,
    /// `ns_drop_tombstones` before the peer refusal and right after it.
    pub tombstones_settled: u64,
    pub tombstones_after_rollback: u64,
    /// The most tier-file unlinks one judged step made (O3's per-step
    /// engagement reads 8).
    pub unlinks_step_max: u64,
    /// Dropped namespaces O3's window read with no file left.
    pub window_dropped: u64,
    /// Retries O6 judged: an attempt after a failure on its path.
    pub retries_judged: u64,
    /// Failed creates O7 read.
    pub failed_creates: u64,
    /// The disk leg's `DISKFULL` refusals whose `used` was the sim disk's.
    pub disk_full_refusals: u64,
    /// O3's windows: the files each opened with (life 1, the life after
    /// the cut) and the life after the cut's R0.
    pub window_files: u64,
    pub reboot_window_files: u64,
    pub reboot_r0_files: u64,
    /// The graceful stop drained every cell, and the dropped namespaces
    /// `RETIRE-STOP` read.
    pub stop_drained: bool,
    pub stop_dropped_read: u64,
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
    /// The drop clause (ADR-0156 D3): every dropped namespace's tier-file
    /// unlink after its `+OK` is the worker's; with it, O3's per-step
    /// clause.
    drops: crate::harness::drop_clause::DropClause,
    /// O6: a failed close's or unlink's retry lies 1 ms after it.
    pace: crate::harness::retry_pace::RetryPace,
    /// O7: failed creates (the create-fault leg).
    creates: crate::harness::create_clause::CreateClause,
    /// O3's window: the life's MANIFEST names, R0 and the bill.
    window: crate::harness::retire_window::RetireWindow,
    /// The drops the harness read before this life's boot.
    life_drops: usize,
    /// The life after the cut's window's R0, which its stop reads again.
    reboot_r0: std::collections::BTreeSet<PathBuf>,
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
        for breach in self.drops.judge(&self.disk) {
            self.fail(breach);
        }
        for breach in self.pace.judge(&self.disk) {
            self.fail(breach);
        }
        for breach in self.creates.judge(&self.disk) {
            self.fail(breach);
        }
        for breach in self.window.observe(&self.disk) {
            self.fail(breach);
        }
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

    /// The tier-file jobs out on every cell, read from the cells' parts.
    fn retire_jobs_out(&self) -> u64 {
        (0..usize::from(self.report.cells))
            .map(|cell| self.node.plane(cell).tier_retire_jobs_out_for_sim())
            .sum()
    }

    /// Tier files of namespace `id` the disk still names.
    fn tier_files_of(&self, id: u32) -> usize {
        let dir = format!("ns-{id}");
        let named = self.disk.names_where(is_tier_file);
        named
            .iter()
            .filter(|path| path.components().any(|part| part.as_os_str() == dir.as_str()))
            .count()
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
        let id = self.node.plane(0).keyspace().ns_get(name).map(|spec| spec.id.0);
        let reply = self.call(client, &[b"INF.NS", b"DROP", name])?;
        if reply != OK {
            return Err(format!("DROP {} answered {reply:?}", String::from_utf8_lossy(name)));
        }
        if let Some(id) = id {
            self.drops.note_drop(id, self.clock.now());
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

    /// Steps until every cell recovered (bounded); from then on O3's and
    /// O7's clauses judge the life's unlinks and creates.
    fn recover(&mut self) -> Result<(), String> {
        self.step_until("recovery", |run| run.node.ready())?;
        self.drops.recovered(&self.disk);
        self.creates.recovered(&self.disk);
        for breach in self.window.begin_life(&self.disk) {
            self.fail(breach);
        }
        Ok(())
    }

    /// Every tier file the disk names.
    fn tier_files_on_disk(&self) -> std::collections::BTreeSet<PathBuf> {
        self.disk.names_where(is_tier_file).into_iter().collect()
    }

    /// The ids of every namespace whose `DROP` the harness read.
    fn dropped_ids(&self) -> std::collections::BTreeSet<u32> {
        self.drops.dropped().collect()
    }

    /// The tier-file jobs out on `cell`.
    fn jobs_out_on(&self, cell: usize) -> u64 {
        self.node.plane(cell).tier_retire_jobs_out_for_sim()
    }

    /// Arms `point` once, as the leg's O6 point.
    fn arm_retry_point(&mut self, point: &'static str) {
        self.pace.armed(point);
        inf_foundation::fault::arm(point, FaultSpec::Nth(1));
    }
}

/// The scenario's node: the m2 durable shape on two or four cells, 64 KiB
/// segments, a checkpoint every 4 MiB of log, a device whose fsyncs take
/// 20 ms against steps of at most 1 ms (the pace leg's, 200 µs).
fn scenario(seed: u64, leg: LifetimesLeg) -> DurableScenario {
    let stall = StallConfig { base_ns: 20_000_000, ..m2_stall_config() };
    let step_ns_max = match leg {
        LifetimesLeg::Pace => PACE_STEP_NS_MAX,
        LifetimesLeg::Full
        | LifetimesLeg::Control
        | LifetimesLeg::CreateFault
        | LifetimesLeg::Disk => 1_000_000,
    };
    DurableScenario {
        cells: if seed.is_multiple_of(2) { 2 } else { 4 },
        segment_bytes: 64 << 10,
        ckpt_interval_bytes: 4 << 20,
        step_ns_max,
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
    run.arm_retry_point(inf_log::fault::TIER_UNLINK_FAIL);
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

/// Phase 4, the held-worker cycle: `victim`, flushed on cell 0, is dropped
/// with the control thread's retirement worker held, so its tier files go
/// out as jobs and stay out; its successor's tiered `CREATE` is refused
/// while they are out (the receipt waits for every job's result). The
/// worker then runs with every unlink failing for `EVERY_ATTEMPT_STEPS`
/// steps, each file retried and never removed, the successor still
/// refused; then the unlinks succeed and the successor is accepted and
/// flushed. An accepted successor while a file of the dropped namespace is
/// on the disk is `HELD-WORKER`; flushed beside the files the held worker
/// keeps open, it also crosses `LIFETIMES`.
fn held_worker_cycle(
    run: &mut Run,
    admin: &mut MiniClient,
    victim: &[u8],
    successor: &[u8],
) -> Result<(), String> {
    let id = run.node.plane(0).keyspace().ns_get(victim).map(|spec| spec.id.0);
    let id = id.ok_or_else(|| "the held-worker cycle's victim is not live".to_string())?;
    run.node.hold_retire = u64::MAX;
    run.drop_ns(admin, victim)?;
    run.step_until("a dropped namespace's tier file out as a job", |run| {
        run.retire_jobs_out() > 0
    })?;
    run.report.jobs_held_across_drop += 1;
    for _ in 0..HELD_TRIES {
        let reply = run.create(admin, successor)?;
        if reply == OK {
            held_worker_accepted(run, id, "with the worker held");
            let flushed = run.flush_on_cell_0(successor);
            run.node.hold_retire = 0;
            return flushed;
        }
        if reply != BUSY {
            return Err(format!("a held-worker cycle's CREATE answered {reply:?}"));
        }
        run.report.refused_busy += 1;
        run.idle(RETRY_GAP_STEPS)?;
    }
    let before = inf_foundation::fault::fired(inf_log::fault::TIER_UNLINK_FAIL);
    inf_foundation::fault::arm(inf_log::fault::TIER_UNLINK_FAIL, FaultSpec::Always);
    run.node.hold_retire = 0;
    let reply = run.idle(EVERY_ATTEMPT_STEPS).and_then(|()| run.create(admin, successor));
    let failed = inf_foundation::fault::fired(inf_log::fault::TIER_UNLINK_FAIL) - before;
    inf_foundation::fault::disarm(inf_log::fault::TIER_UNLINK_FAIL);
    run.report.unlink_every_attempt += failed;
    run.report.unlink_failures += failed;
    match reply? {
        reply if reply == OK => {
            held_worker_accepted(run, id, "with every unlink failing");
            return run.flush_on_cell_0(successor);
        }
        reply if reply == BUSY => run.report.refused_busy += 1,
        reply => return Err(format!("a failing-unlink CREATE answered {reply:?}")),
    }
    run.create_until_ok(admin, successor)?;
    run.flush_on_cell_0(successor)
}

/// The held-worker cycle's successor answered `+OK`: a breach while the
/// disk still names a tier file of the dropped namespace `id`.
fn held_worker_accepted(run: &mut Run, id: u32, while_: &str) {
    let left = run.tier_files_of(id);
    if left > 0 {
        run.fail(format!(
            "HELD-WORKER: the successor's CREATE answered +OK {while_}, {left} tier files of \
             dropped namespace {id} still on the disk"
        ));
    }
}

/// Phase 4b, the full pass (O3's per-step engagement): `FULL_PASS_VICTIMS`
/// live namespaces, each flushed on cell 1 beside its file on cell 0, are
/// dropped with the worker held, until cells 0 and 1 each hold their 8 job
/// positions out; the worker then runs, and one pass may unlink only 8 of
/// the 16 ready (the per-step clause reads each step). Their successors
/// restore the node's 64 lifetimes.
fn full_pass_cycle(run: &mut Run, admin: &mut MiniClient) -> Result<(), String> {
    let victims: Vec<Vec<u8>> = (3..3 + FULL_PASS_VICTIMS).map(|index| name("t", index)).collect();
    let mut ids = Vec::new();
    for victim in &victims {
        let before = run.dirs_on(1);
        run.write_until(victim, 1, "flushed on cell 1", move |run| run.dirs_on(1) > before)?;
        let id = run.node.plane(0).keyspace().ns_get(victim).map(|spec| spec.id.0);
        ids.push(id.ok_or_else(|| "a full-pass victim is not live".to_string())?);
    }
    run.node.hold_retire = u64::MAX;
    for victim in &victims {
        run.drop_ns(admin, victim)?;
    }
    let full = u64::try_from(inf_foundation::limits::TIER_RETIRE_JOBS_PER_CELL).unwrap_or(8);
    let held = run.step_until("two cells' job positions out", move |run| {
        run.jobs_out_on(0) == full && run.jobs_out_on(1) == full
    });
    if let Err(err) = held {
        run.node.hold_retire = 0;
        return Err(err);
    }
    let window = open_window(run, "RETIRE")?;
    run.report.window_files = window.bill.files;
    close_window(run, &window)?;
    run.step_until("the full pass's files gone", move |run| {
        ids.iter().all(|&id| run.tier_files_of(id) == 0)
    })?;
    for index in 0..FULL_PASS_VICTIMS {
        run.create_until_ok(admin, &name("q", index))?;
    }
    Ok(())
}

/// One retirement window's bill and where it opened.
struct Window {
    label: &'static str,
    bill: crate::harness::retire_window::WindowBill,
    /// The disk log's length at the opening.
    from: usize,
}

/// Whether every cell is quiet for a window's opening: no round out, no
/// cold read, no extent hold or unsettled fence, every drop durable, the
/// ledger settled and no checkpoint transition, and no harness hold but
/// the worker's.
fn node_quiet(run: &Run) -> bool {
    let cells = (0..usize::from(run.report.cells)).all(|cell| {
        let (tier, ledger, idle) = run.node.plane(cell).quiescence_for_sim();
        tier.quiet() && ledger && idle
    });
    cells && run.node.frozen.is_none() && !run.node.hold_inbox
}

/// Whether every injected close or unlink failure the disk logged lies at
/// least 1 ms plus a step before now, so every retry is due at the opening.
fn retries_due(run: &Run) -> bool {
    let points = [inf_log::fault::TIER_UNLINK_FAIL, inf_log::fault::TIER_CLOSE_FAIL];
    let last = run
        .disk
        .logged_calls(0)
        .iter()
        .filter(
            |entry| matches!(entry.result, CallResult::Injected(point) if points.contains(&point)),
        )
        .filter_map(|entry| entry.mark.map(|mark| mark.at))
        .max();
    let a = run.step_ns_max + 1_000;
    last.is_none_or(|at| run.clock.now().0 >= at.0.saturating_add(1_000_000 + a))
}

/// Opens a retirement window (O3): the worker held, the node stepped until
/// it is quiet with every retry due, then the worker released and, with no
/// step between, the hold and the quiescence read again and the bill
/// snapshot from the disk. Never quiet, or a hold at the instant of opening,
/// is `RETIRE-OPEN`.
fn open_window(run: &mut Run, label: &'static str) -> Result<Window, String> {
    run.node.hold_retire = u64::MAX;
    let begun = run.clock.now();
    while !(node_quiet(run) && retries_due(run)) {
        if run.clock.now().0.saturating_sub(begun.0) > WINDOW_OPEN_NS_MAX {
            run.node.hold_retire = 0;
            let cells: Vec<String> = (0..usize::from(run.report.cells))
                .map(|cell| format!("{:?}", run.node.plane(cell).quiescence_for_sim()))
                .collect();
            return Err(format!(
                "RETIRE-OPEN: the node was never quiet with every retry due in \
                 {WINDOW_OPEN_NS_MAX} ns: cells {cells:?}, retries due {}",
                retries_due(run)
            ));
        }
        run.step()?;
    }
    // Canary `inf_canary_retire_window_held_open` (a premise plant, read in
    // the simulator alone): the window opens with the worker still held.
    if !cfg!(inf_canary_retire_window_held_open) {
        run.node.hold_retire = 0;
    }
    if run.node.hold_retire != 0 || !node_quiet(run) {
        let hold = run.node.hold_retire;
        run.node.hold_retire = 0;
        return Err(format!(
            "RETIRE-OPEN: a hold at the window's opening: hold_retire {hold}, frozen {:?}, inbox \
             held {}, quiet {}",
            run.node.frozen,
            run.node.hold_inbox,
            node_quiet(run)
        ));
    }
    // D is the files of the namespaces dropped in this life: a dropped
    // namespace's file a cut resurrected after its tombstone retired has no
    // owner in this life, so no sweep or window removes it.
    let life: std::collections::BTreeSet<u32> = run.drops.dropped().skip(run.life_drops).collect();
    let bill = run.window.bill(&run.tier_files_on_disk(), &life, &run.dropped_ids());
    Ok(Window { label, bill, from: run.disk.logged_calls(0).len() })
}

/// Runs a window's steps and closes it: after `⌈F / 8⌉ + 5c + 1` steps (one
/// more for each 8 files created inside it), no file of D is on the disk
/// (`<label>-BOUND`) and none of R0 (`RETIRE-R0`). Its engagement is F > 0.
fn close_window(run: &mut Run, window: &Window) -> Result<(), String> {
    let bill = &window.bill;
    let mut steps = 0u64;
    loop {
        let created = run
            .disk
            .logged_calls(window.from)
            .iter()
            .filter(|entry| {
                matches!(entry.call, DiskCall::CreateTier { .. }) && entry.result == CallResult::Ok
            })
            .count() as u64;
        if steps >= (bill.steps + created.div_ceil(8)).min(WINDOW_STEPS_MAX) {
            break;
        }
        run.step()?;
        steps += 1;
    }
    let on_disk = run.tier_files_on_disk();
    let left = bill.d.iter().filter(|path| on_disk.contains(*path)).count();
    let label = window.label;
    if left > 0 {
        run.fail(format!(
            "{label}-BOUND: {left} of {} files left after {steps} steps (F {}, c {})",
            bill.files, bill.files, bill.shards
        ));
    }
    for path in bill.r0.iter().filter(|path| on_disk.contains(*path)) {
        run.fail(format!(
            "RETIRE-R0: {} a landed swap retired is on the disk after {label}'s {steps} steps",
            path.display()
        ));
    }
    if bill.files == 0 {
        run.fail(format!("VACUOUS {label}-BOUND: the window opened with no file to retire"));
    }
    Ok(())
}

/// O3's window on the full leg: once every dropped owner on every cell has
/// drained (the caller's `wait_positions`), no tier file of a namespace
/// dropped in this life is on the disk.
fn retire_window(run: &mut Run) {
    let dropped: Vec<u32> = run.drops.dropped().collect();
    for id in dropped {
        let left = run.tier_files_of(id);
        if left > 0 {
            run.fail(format!(
                "RETIRE-WINDOW: {left} tier files of dropped namespace {id} on the disk once \
                 every dropped owner drained"
            ));
        } else {
            run.report.window_dropped += 1;
        }
    }
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

/// Phase 5: each cycle drops `victim` (every `CHURN_BLOB_EVERY`th with blob
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

/// Phase 6: the four disconnects at 63 grants. After each, the node is back
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

/// Phase 7: the peer refusal at 63 grants, with the tombstone count settled
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

/// A booted leg's run on `scenario`, or the report of its refused boot.
fn new_run(
    scenario: &DurableScenario,
    seed: u64,
    leg: LifetimesLeg,
    observer: &TraceObserver,
    clock: &Rc<VirtualClock>,
) -> Result<Run, Box<NsLifetimesReport>> {
    let disk = build_disk(seed, scenario.stall.as_ref());
    let report = empty_report(scenario.cells, leg);
    let node = match boot(scenario, PathBuf::from("node"), &disk, clock, observer) {
        Ok(node) => node,
        Err(err) => {
            let mut report = report;
            report.violations.push(format!("boot failed: {err}"));
            return Err(Box::new(report));
        }
    };
    Ok(Run {
        node,
        rng: SplitMix64::new(seed ^ 0x11FE_7135),
        clock: Rc::clone(clock),
        disk: disk.clone(),
        step_ns_max: scenario.step_ns_max,
        report,
        last_committed: inf_store::FIRST_NAMED_NS_ID - 1,
        live: BTreeMap::new(),
        refused: Vec::new(),
        next_key: 0,
        drops: crate::harness::drop_clause::DropClause::per_step(),
        pace: crate::harness::retry_pace::RetryPace::default(),
        creates: crate::harness::create_clause::CreateClause::default(),
        window: crate::harness::retire_window::RetireWindow::new(
            PathBuf::from("node"),
            scenario.cells,
        ),
        life_drops: 0,
        reboot_r0: std::collections::BTreeSet::new(),
    })
}

/// Runs one seeded `m4-ns-lifetimes` leg.
fn run_observed(seed: u64, leg: LifetimesLeg, observer: TraceObserver) -> NsLifetimesReport {
    let scenario = scenario(seed, leg);
    let clock = Rc::new(VirtualClock::new(Nanos(1)));
    let mut run = match new_run(&scenario, seed, leg, &observer, &clock) {
        Ok(run) => run,
        Err(report) => return finish(*report, &observer, &clock),
    };
    if let Err(err) = phases(&mut run) {
        run.fail(err);
        return finish(run.report, &observer, &clock);
    }
    match leg {
        LifetimesLeg::Control => control_checks(&mut run),
        LifetimesLeg::Pace | LifetimesLeg::CreateFault | LifetimesLeg::Disk => {
            leg_engagement(&mut run);
        }
        LifetimesLeg::Full => return reboot(run, &scenario, seed, &observer, &clock),
    }
    finish(fold_report(run), &observer, &clock)
}

/// The cut and the reboot: the old node is gone before the disk is cut;
/// then phase 8 and the engagement verdict.
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
        mut drops,
        mut pace,
        creates,
        window,
        life_drops: _,
        reboot_r0,
    } = run;
    let life_drops = drops.dropped().count();
    drop(node);
    drops.reboot();
    pace.reboot(&disk);
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
        drops,
        pace,
        creates,
        window,
        life_drops,
        reboot_r0,
    };
    if let Err(err) = after_cut(&mut run) {
        run.fail(err);
    }
    engagement(&mut run);
    finish(fold_report(run), observer, clock)
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
        jobs_held_across_drop: 0,
        unlink_every_attempt: 0,
        churn_cycles: 0,
        disconnects: 0,
        refused_leg: 0,
        tombstones_settled: 0,
        tombstones_after_rollback: 0,
        unlinks_step_max: 0,
        window_dropped: 0,
        retries_judged: 0,
        failed_creates: 0,
        disk_full_refusals: 0,
        window_files: 0,
        reboot_window_files: 0,
        reboot_r0_files: 0,
        stop_drained: false,
        stop_dropped_read: 0,
    }
}

/// The run's clause counts into its report.
fn fold_report(run: Run) -> NsLifetimesReport {
    let mut report = run.report;
    report.unlinks_step_max = run.drops.unlinks_step_max();
    report.retries_judged = run.pace.retries;
    report
}

/// The pace and create-fault legs' engagement: O6's points, O7's points.
fn leg_engagement(run: &mut Run) {
    let vacuous = match run.report.leg {
        LifetimesLeg::Pace | LifetimesLeg::Disk => run.pace.engagement(),
        LifetimesLeg::CreateFault => run.creates.engagement(),
        LifetimesLeg::Full | LifetimesLeg::Control => Vec::new(),
    };
    for what in vacuous {
        run.fail(what);
    }
}

/// Phases 1–7 on one life (the control leg: phase 1 alone; the pace and
/// create-fault legs their own).
fn phases(run: &mut Run) -> Result<(), String> {
    run.recover()?;
    let mut admin = MiniClient::connect(&mut run.node, 0);
    match run.report.leg {
        LifetimesLeg::Pace => return pace_leg(run, &mut admin),
        LifetimesLeg::CreateFault => return create_fault_leg(run, &mut admin),
        LifetimesLeg::Disk => return disk_leg(run, &mut admin),
        LifetimesLeg::Full | LifetimesLeg::Control => {}
    }
    fill(run, &mut admin)?;
    if run.report.leg == LifetimesLeg::Control {
        return run.idle(256);
    }
    extent_cycle(run, &mut admin, b"t00", b"n00")?;
    pinned_cycle(run, &mut admin, b"t01", b"n01")?;
    held_worker_cycle(run, &mut admin, b"t02", b"n02")?;
    full_pass_cycle(run, &mut admin)?;
    let last = churn(run, &mut admin, b"n02".to_vec())?;
    run.drop_ns(&mut admin, &last)?;
    run.wait_positions(LIFETIMES as u64 - 1)?;
    retire_window(run);
    disconnects(run, &mut admin)?;
    peer_refusal(run, &mut admin)
}

/// O6's pace leg: one namespace flushed on cell 0, its one tier file
/// dropped with `tier_close_fail` and `tier_unlink_fail` armed once: the
/// worker's close fails, then its retry's unlink, then the next unlink
/// removes the file. O6 judges each retry's distance from its failure.
fn pace_leg(run: &mut Run, admin: &mut MiniClient) -> Result<(), String> {
    let ns = b"s00".to_vec();
    if run.create(admin, &ns)? != OK {
        return Err("the pace leg's CREATE was refused".to_string());
    }
    run.flush_on_cell_0(&ns)?;
    let id = run.node.plane(0).keyspace().ns_get(&ns).map(|spec| spec.id.0);
    let id = id.ok_or_else(|| "the pace leg's namespace is not live".to_string())?;
    run.arm_retry_point(inf_log::fault::TIER_CLOSE_FAIL);
    run.arm_retry_point(inf_log::fault::TIER_UNLINK_FAIL);
    run.drop_ns(admin, &ns)?;
    let gone = run.step_until("the paced file gone", move |run| run.tier_files_of(id) == 0);
    let fired = [inf_log::fault::TIER_CLOSE_FAIL, inf_log::fault::TIER_UNLINK_FAIL]
        .map(inf_foundation::fault::fired);
    inf_foundation::fault::disarm_all();
    gone?;
    if fired != [1, 1] {
        run.fail(format!(
            "VACUOUS RETRY-PACE: the pace leg's points fired {fired:?}, not once each"
        ));
    }
    run.report.unlink_failures += fired[1];
    run.wait_positions(0)
}

/// Writes to `ns` on cell 0, the worker held, until `point` has failed a
/// create; then holds the residue's unlink `RESIDUE_HELD_STEPS` more steps,
/// each a MAINTAIN with the residue's job out, and releases the worker.
fn fail_held_create(run: &mut Run, ns: &[u8], point: &'static str) -> Result<(), String> {
    inf_foundation::fault::arm(point, FaultSpec::Nth(1));
    run.node.hold_retire = u64::MAX;
    let failed = run
        .write_until(ns, 0, "met a create failure", move |_| {
            inf_foundation::fault::fired(point) > 0
        })
        .and_then(|()| run.idle(RESIDUE_HELD_STEPS));
    let out = run.jobs_out_on(0);
    run.node.hold_retire = 0;
    inf_foundation::fault::disarm(point);
    failed?;
    if out > 0 {
        run.creates.held(point);
    }
    Ok(())
}

/// O7's create-fault leg: `f00`'s first create refused before its file
/// exists, `f01`'s after, each residue's unlink held across a MAINTAIN;
/// `f02`'s failing on every attempt `CREATE_EVERY_ATTEMPTS` times; `f03`
/// dropped while its failed create's residue is out. Each namespace's file
/// is then made under the name its failed create named.
fn create_fault_leg(run: &mut Run, admin: &mut MiniClient) -> Result<(), String> {
    let points = [inf_log::fault::TIER_CREATE_FAIL, inf_log::fault::TIER_CREATE_POST_FAIL];
    for (index, point) in points.into_iter().enumerate() {
        let ns = name("f", index as u32);
        if run.create(admin, &ns)? != OK {
            return Err("a create-fault leg's CREATE was refused".to_string());
        }
        let before = run.dirs_on(0);
        fail_held_create(run, &ns, point)?;
        run.step_until("the failed create's name made", move |run| run.dirs_on(0) > before)?;
    }
    every_attempt(run, admin)?;
    dropped_with_residue_out(run, admin)?;
    run.idle(SETTLE_GAP_STEPS)?;
    for breach in run.creates.left_files(&run.disk) {
        run.fail(breach);
    }
    run.report.failed_creates = run.creates.failed_creates;
    Ok(())
}

/// `f02`'s create on every attempt: `tier_create_post_fail` armed always
/// until it fired `CREATE_EVERY_ATTEMPTS` times, one residue at a time,
/// then disarmed; the next create makes the name every failure named.
fn every_attempt(run: &mut Run, admin: &mut MiniClient) -> Result<(), String> {
    let point = inf_log::fault::TIER_CREATE_POST_FAIL;
    let ns = name("f", 2);
    if run.create(admin, &ns)? != OK {
        return Err("the every-attempt CREATE was refused".to_string());
    }
    let before = run.dirs_on(0);
    let fired_before = inf_foundation::fault::fired(point);
    inf_foundation::fault::arm(point, FaultSpec::Always);
    let failed = move |_: &Run| inf_foundation::fault::fired(point) > fired_before;
    let mut tried = run.write_until(&ns, 0, "met a create failure", failed);
    let mut steps = 0u64;
    while tried.is_ok()
        && inf_foundation::fault::fired(point) - fired_before < CREATE_EVERY_ATTEMPTS
    {
        if steps == CREATE_EVERY_ATTEMPT_STEPS {
            tried = Err(format!(
                "VACUOUS FAILED-CREATE: {} of {CREATE_EVERY_ATTEMPTS} failed creates in \
                 {CREATE_EVERY_ATTEMPT_STEPS} steps",
                inf_foundation::fault::fired(point) - fired_before
            ));
            break;
        }
        tried = run.step();
        steps += 1;
    }
    inf_foundation::fault::disarm(point);
    tried?;
    run.step_until("the every-attempt name made", move |run| run.dirs_on(0) > before)
}

/// `f03` dropped while its failed create's residue is out: its owner drains
/// only once the residue's result is consumed, and no file of it is left.
fn dropped_with_residue_out(run: &mut Run, admin: &mut MiniClient) -> Result<(), String> {
    let point = inf_log::fault::TIER_CREATE_POST_FAIL;
    let ns = name("f", 3);
    if run.create(admin, &ns)? != OK {
        return Err("the dropped residue's CREATE was refused".to_string());
    }
    let fired_before = inf_foundation::fault::fired(point);
    inf_foundation::fault::arm(point, FaultSpec::Nth(1));
    run.node.hold_retire = u64::MAX;
    let failed = move |_: &Run| inf_foundation::fault::fired(point) > fired_before;
    let tried = run.write_until(&ns, 0, "met a create failure", failed);
    inf_foundation::fault::disarm(point);
    let id = run.node.plane(0).keyspace().ns_get(&ns).map(|spec| spec.id.0);
    let dropped = tried.and_then(|()| run.drop_ns(admin, &ns));
    run.node.hold_retire = 0;
    dropped?;
    run.wait_positions(3)?;
    if let Some(id) = id
        && run.tier_files_of(id) > 0
    {
        run.fail(format!("FAILED-CREATE: a file of {id}, dropped with its residue out, is left"));
    }
    Ok(())
}

/// The bytes of namespace `id`'s tier files under `cell`'s shard, read from
/// the sim disk's own files (each file's length).
fn disk_tier_bytes(run: &Run, cell: usize, id: u32) -> u64 {
    let (shard, dir) = (format!("shard-{cell}"), format!("ns-{id}"));
    let named = run.disk.names_where(is_tier_file);
    let under = |path: &&PathBuf| {
        let parts: Vec<_> = path.components().map(|part| part.as_os_str().to_owned()).collect();
        parts.iter().any(|part| *part == *shard) && parts.iter().any(|part| *part == *dir)
    };
    named
        .iter()
        .filter(under)
        .filter_map(|path| {
            use inf_log::fs::{SegmentFile, SegmentFs};
            run.disk.open_read(path).ok()?.file_size().ok()
        })
        .sum()
}

/// The disk row (`RETIRE-CHARGE`), O6's points armed: a live namespace on
/// cell 0 fills until its first tier file seals, every key is deleted, and
/// checkpoints retire the dead file while `tier_unlink_fail` fails each of
/// its unlinks. With the budget clamped to the sim disk's bytes of the
/// namespace's tier files, a write is refused `DISKFULL` and the refusal's
/// `used` is those bytes: the retiring file is still counted. Disarmed, the
/// worker unlinks it, and a write is admitted again.
fn disk_leg(run: &mut Run, admin: &mut MiniClient) -> Result<(), String> {
    let ns = b"d00".to_vec();
    if run.create(admin, &ns)? != OK {
        return Err("the disk leg's CREATE was refused".to_string());
    }
    let id = run.node.plane(0).keyspace().ns_get(&ns).map(|spec| spec.id.0);
    let id = id.ok_or_else(|| "the disk leg's namespace is not live".to_string())?;
    let mut client = run.use_ns(0, &ns)?;
    let mut written = Vec::new();
    for _ in 0..DISK_FILL_BATCHES_MAX {
        if run.tier_files_of(id) >= 2 {
            break;
        }
        let keys = run.owned_keys(0, "d", FLUSH_BATCH);
        run.write_inline(&mut client, &keys)?;
        written.extend(keys);
        run.idle(FLUSH_SETTLE_STEPS)?;
    }
    if run.tier_files_of(id) < 2 {
        return Err("VACUOUS RETIRE-CHARGE: the disk leg's first tier file never sealed".into());
    }
    for key in &written {
        client.send(&mut run.node, &[b"DEL", key]);
    }
    for _ in &written {
        run.reply(&mut client)?;
    }
    run.pace.armed(inf_log::fault::TIER_UNLINK_FAIL);
    inf_foundation::fault::arm(inf_log::fault::TIER_UNLINK_FAIL, FaultSpec::Always);
    let charged = disk_charge(run, admin, &mut client, id, &ns);
    run.report.unlink_failures += inf_foundation::fault::fired(inf_log::fault::TIER_UNLINK_FAIL);
    inf_foundation::fault::disarm(inf_log::fault::TIER_UNLINK_FAIL);
    charged?;
    disk_release(run, &mut client, id)
}

/// The disk leg's clamp, the retired file failing: checkpoints until a
/// tier file of the namespace failed its unlink, then the budget set to the
/// sim disk's bytes of its files, and a write refused with those bytes.
fn disk_charge(
    run: &mut Run,
    admin: &mut MiniClient,
    client: &mut MiniClient,
    id: u32,
    ns: &[u8],
) -> Result<(), String> {
    let point = CallResult::Injected(inf_log::fault::TIER_UNLINK_FAIL);
    let dir = format!("ns-{id}");
    let from = run.disk.logged_calls(0).len();
    let failed = move |run: &Run| {
        run.disk.logged_calls(from).iter().any(|entry| {
            entry.call == DiskCall::UnlinkTier
                && entry.result == point
                && entry.path.components().any(|part| part.as_os_str() == dir.as_str())
        })
    };
    for _ in 0..DISK_CKPTS_MAX {
        if failed(run) {
            break;
        }
        let reply = run.call(admin, &[b"INF.CKPT", b"WAIT"])?;
        if reply != OK {
            return Err(format!("INF.CKPT WAIT answered {reply:?}"));
        }
        run.idle(SETTLE_GAP_STEPS)?;
    }
    if !failed(run) {
        return Err(
            "VACUOUS RETIRE-CHARGE: no retired file of the disk leg failed its unlink".into()
        );
    }
    run.idle(SETTLE_GAP_STEPS)?;
    let actual = disk_tier_bytes(run, 0, id);
    let budget = actual.to_string().into_bytes();
    let reply = run.call(admin, &[b"INF.NS", b"SET", ns, b"DISK-BUDGET", &budget])?;
    if reply != OK {
        return Err(format!("DISK-BUDGET {actual} answered {reply:?}"));
    }
    // The clamp reaches admission at the namespace's next MAINTAIN.
    run.idle(SETTLE_GAP_STEPS)?;
    let value = vec![b'v'; INLINE_BYTES];
    for _ in 0..DISK_WRITES_MAX {
        let key = run.owned_keys(0, "dc", 1).remove(0);
        let reply = run.call(client, &[b"SET", &key, &value])?;
        let now = disk_tier_bytes(run, 0, id);
        if !reply.starts_with(b"-DISKFULL") {
            run.fail(format!(
                "RETIRE-CHARGE: a write answered {:?} at a budget of the sim disk's {actual} \
                 bytes, the retired file failing its unlink on the disk",
                String::from_utf8_lossy(&reply)
            ));
            return Ok(());
        }
        let expected = format!("(used={now} budget={actual})");
        if String::from_utf8_lossy(&reply).contains(&expected) {
            run.report.disk_full_refusals += 1;
        } else {
            run.fail(format!(
                "RETIRE-CHARGE: the refusal {:?} counts other than the sim disk's {now} bytes of \
                 the namespace's tier files",
                String::from_utf8_lossy(&reply)
            ));
            return Ok(());
        }
        run.idle(1)?;
    }
    Ok(())
}

/// The disk leg's release: the plant disarmed, the worker unlinks the
/// retired file at its next retry, and a write is admitted again.
fn disk_release(run: &mut Run, client: &mut MiniClient, id: u32) -> Result<(), String> {
    let before = run.tier_files_of(id);
    run.step_until("the retired file unlinked", move |run| run.tier_files_of(id) < before)?;
    let value = vec![b'v'; INLINE_BYTES];
    for _ in 0..DISK_WRITES_MAX {
        let key = run.owned_keys(0, "dr", 1).remove(0);
        let reply = run.call(client, &[b"SET", &key, &value])?;
        if reply == OK {
            return Ok(());
        }
        if !reply.starts_with(b"-DISKFULL") {
            return Err(format!("a released write answered {reply:?}"));
        }
        run.idle(SETTLE_GAP_STEPS)?;
    }
    run.fail("RETIRE-CHARGE: admission never reopened once the retired file was gone".to_string());
    Ok(())
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

/// Phase 8, after the cut: every live name on every cell, the refused ones
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
    let held = reboot_r0(run);
    if let Err(err) = held {
        run.node.hold_retire = 0;
        return Err(err);
    }
    reboot_window(run, &mut admin)?;
    run.drop_ns(&mut admin, b"p00")?;
    run.create_until_ok(&mut admin, b"z00")?;
    // Canary `inf_canary_retire_drop_at_stop` (a premise plant, read in the
    // simulator alone): just before the stop, a tier file no owner holds is
    // made on the disk under a namespace dropped in this life. A flushed
    // namespace dropped there instead is retired by the stop's own drain, so
    // only a file outside every owner breaks the premise that each dropped
    // file passed through the window.
    if cfg!(inf_canary_retire_drop_at_stop) {
        plant_stray_file(run)?;
    }
    admin.close(&mut run.node);
    graceful_stop(run)?;
    retire_stop(run);
    Ok(())
}

/// Up to `count` live namespaces with keys written on cell 0 and a tier
/// file the recovered MANIFESTs name, in name order.
fn flushed_live(run: &Run, count: usize) -> Vec<Vec<u8>> {
    let named = run.window.named_ns();
    let mut names = Vec::new();
    for (name, keys) in &run.live {
        if names.len() == count {
            break;
        }
        let id = run.node.plane(0).keyspace().ns_get(name).map(|spec| spec.id.0);
        if !keys.is_empty() && id.is_some_and(|id| named.contains(&id)) {
            names.push(name.clone());
        }
    }
    names
}

/// The life after the cut's R0 (O3): a live namespace whose file the
/// recovered MANIFEST names has every key deleted, and with the worker held
/// checkpoints are taken until a landed swap retired the dead file, which
/// the disk still holds and the latest MANIFEST no longer names.
fn reboot_r0(run: &mut Run) -> Result<(), String> {
    let Some(name) = flushed_live(run, 1).pop() else {
        return Err("VACUOUS RETIRE-R0: no live namespace with a manifested file".to_string());
    };
    let keys = run.live.get(&name).cloned().unwrap_or_default();
    let mut client = run.use_ns(0, &name)?;
    for key in &keys {
        client.send(&mut run.node, &[b"DEL", key]);
    }
    for _ in &keys {
        run.reply(&mut client)?;
    }
    client.close(&mut run.node);
    if let Some(written) = run.live.get_mut(&name) {
        written.clear();
    }
    run.node.hold_retire = u64::MAX;
    let mut admin = MiniClient::connect(&mut run.node, 0);
    for _ in 0..R0_CKPTS_MAX {
        let r0 = run.window.r0(&run.tier_files_on_disk(), &run.dropped_ids());
        if !r0.is_empty() {
            break;
        }
        let reply = run.call(&mut admin, &[b"INF.CKPT", b"WAIT"])?;
        if reply != OK {
            return Err(format!("INF.CKPT WAIT answered {reply:?}"));
        }
        run.idle(SETTLE_GAP_STEPS)?;
    }
    admin.close(&mut run.node);
    Ok(())
}

/// The life after the cut's window (O3, `REBOOT-BOUND`): `REBOOT_VICTIMS`
/// flushed live namespaces dropped with the worker held beside R0's file,
/// then the window opened and closed. Its R0 engagement: the window opened
/// with R0 nonempty, else `VACUOUS RETIRE-R0`.
fn reboot_window(run: &mut Run, admin: &mut MiniClient) -> Result<(), String> {
    run.node.hold_retire = u64::MAX;
    for name in flushed_live(run, REBOOT_VICTIMS) {
        if let Err(err) = run.drop_ns(admin, &name) {
            run.node.hold_retire = 0;
            return Err(err);
        }
    }
    let window = open_window(run, "REBOOT")?;
    run.report.reboot_window_files = window.bill.files;
    run.report.reboot_r0_files = window.bill.r0.len() as u64;
    let r0_empty = window.bill.r0.is_empty();
    close_window(run, &window)?;
    run.reboot_r0 = window.bill.r0;
    if r0_empty {
        run.fail("VACUOUS RETIRE-R0: the window after the cut opened with no R0 file".to_string());
    }
    Ok(())
}

/// The stop plant's file: a tier file no owner holds, under the last
/// namespace dropped in this life, its handle closed at once.
fn plant_stray_file(run: &mut Run) -> Result<(), String> {
    use inf_log::fs::SegmentFs;
    let id = run.drops.dropped().skip(run.life_drops).last();
    let id =
        id.ok_or_else(|| "the stop plant found no namespace dropped in the life".to_string())?;
    let cold = PathBuf::from("node").join("shard-0").join(format!("ns-{id}")).join("cold");
    run.disk.create_dir_all(&cold).map_err(|err| format!("the stop plant's dir: {err}"))?;
    let path = cold.join(inf_log::tier_file_name(u32::MAX - 1));
    let file = run.disk.create_tier(&path, inf_log::TierIoMode::Buffered);
    drop(file.map_err(|err| format!("the stop plant's file: {err}"))?);
    Ok(())
}

/// A graceful stop (ADR-0124 D2): every cell asked to stop, the assembly's
/// `finish_stop` once every cell is Quiet, then steps until every cell is
/// Drained (bounded).
fn graceful_stop(run: &mut Run) -> Result<(), String> {
    let cells = usize::from(run.report.cells);
    for cell in 0..cells {
        run.node.plane_mut(cell).request_stop();
    }
    let mut finished = false;
    for _ in 0..STOP_STEPS_MAX {
        let phases: Vec<inf_server::StopPhase> =
            (0..cells).map(|cell| run.node.plane(cell).stop_phase()).collect();
        if phases.iter().all(|phase| *phase == inf_server::StopPhase::Drained) {
            run.report.stop_drained = true;
            return Ok(());
        }
        let quiet = phases.iter().all(|phase| {
            matches!(phase, inf_server::StopPhase::Quiet | inf_server::StopPhase::Drained)
        });
        if quiet && !finished {
            for cell in 0..cells {
                run.node.plane_mut(cell).finish_stop();
            }
            finished = true;
        }
        run.step()?;
    }
    run.report.stalled = true;
    Err(format!("the graceful stop never drained in {STOP_STEPS_MAX} steps"))
}

/// `RETIRE-STOP` (O3): after the graceful stop, no tier file of a namespace
/// dropped since the life's boot, and none of the window's R0, is on the
/// disk. Engagement: the stop drained and a dropped namespace was read.
fn retire_stop(run: &mut Run) {
    let on_disk = run.tier_files_on_disk();
    let dropped: Vec<u32> = run.drops.dropped().skip(run.life_drops).collect();
    for &id in &dropped {
        let dir = format!("ns-{id}");
        let left = on_disk
            .iter()
            .filter(|path| path.components().any(|part| part.as_os_str() == dir.as_str()))
            .count();
        if left > 0 {
            run.fail(format!(
                "RETIRE-STOP: {left} tier files of namespace {id}, dropped in this life, on the \
                 disk after the graceful stop"
            ));
        }
    }
    let r0_left: Vec<PathBuf> =
        run.reboot_r0.iter().filter(|path| on_disk.contains(*path)).cloned().collect();
    for path in r0_left {
        run.fail(format!("RETIRE-STOP: R0's {} on the disk after the stop", path.display()));
    }
    run.report.stop_dropped_read = dropped.len() as u64;
    if !run.report.stop_drained || dropped.is_empty() {
        run.fail(format!(
            "VACUOUS RETIRE-STOP: the stop drained {}, {} dropped namespaces read",
            run.report.stop_drained,
            dropped.len()
        ));
    }
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
        (r.jobs_held_across_drop > 0, "a DROP answered with its tier files held out as jobs"),
        (r.unlink_every_attempt > 1, "a drained tier file's every attempt failed"),
        (r.churn_cycles == u64::from(CHURN), "the churn ran every cycle"),
        (r.disconnects == 4, "every disconnect closed with its program running"),
    ];
    let missed: Vec<&str> = clauses.iter().filter(|(met, _)| !met).map(|(_, what)| *what).collect();
    for what in missed {
        run.fail(format!("VACUOUS: not met: {what}"));
    }
    if let Some(vacuous) = run.drops.engagement() {
        run.fail(vacuous);
    }
    if let Some(vacuous) = run.drops.step_engagement() {
        run.fail(vacuous);
    }
    if run.report.window_files == 0 {
        run.fail("VACUOUS RETIRE-BOUND: the full pass's window had no file to retire".to_string());
    }
    if run.report.window_dropped == 0 {
        run.fail("VACUOUS RETIRE-WINDOW: no dropped namespace was read at the window".to_string());
    }
    for vacuous in run.pace.engagement() {
        run.fail(vacuous);
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

#[cfg(test)]
mod tests {
    use super::*;
    use inf_runtime::OwnerPhase;

    /// Steps TF1 waits for a disk entry or a role's phase: the run's own
    /// step bound, far above the few steps each needs.
    const TF1_STEPS_MAX: u32 = 4_096;

    /// The position of the first entry from `from` on whose path is `path`
    /// and which `wanted` accepts, stepping the run until it is logged.
    fn await_entry(
        run: &mut Run,
        from: usize,
        what: &str,
        wanted: impl Fn(&inf_log::fs::sim::LoggedCall) -> bool,
    ) -> inf_log::fs::sim::LoggedCall {
        for _ in 0..TF1_STEPS_MAX {
            if let Some(entry) = run.disk.logged_calls(from).into_iter().find(|entry| wanted(entry))
            {
                return entry;
            }
            run.step().expect("a step");
        }
        panic!("TF1: {what} never logged in {TF1_STEPS_MAX} steps");
    }

    /// TF1, the retirement role (ADR-0154 A1, ADR-0156 D3): an idle durable
    /// cell whose one retiring owner, a dropped namespace's tier file,
    /// failed its unlink once. At the step after the worker's failed
    /// attempt the cell has consumed the `Retry`: its role is Armed on the
    /// cell's timer set at a deadline in [due, due + 1 ms] (the set rounds
    /// up to whole milliseconds), the heap's earliest deadline at or before
    /// it. The walk then offers the file again once the delivery's turn is
    /// past its due, and the worker unlinks it, `Ok`, at or after the due.
    #[test]
    fn tf1_a_retried_files_due_arms_the_retirement_role() {
        const SEED: u64 = 0x2;
        let leg = LifetimesLeg::Pace;
        let scenario = scenario(SEED, leg);
        let clock = Rc::new(VirtualClock::new(Nanos(1)));
        let observer = TraceObserver::default();
        let Ok(mut run) = new_run(&scenario, SEED, leg, &observer, &clock) else {
            panic!("TF1: the node's boot was refused");
        };
        run.recover().expect("recovered");
        let mut admin = MiniClient::connect(&mut run.node, 0);
        assert_eq!(run.create(&mut admin, b"r00").expect("a reply"), OK);
        run.flush_on_cell_0(b"r00").expect("flushed");
        let from = run.disk.logged_calls(0).len();
        inf_foundation::fault::arm(inf_log::fault::TIER_UNLINK_FAIL, FaultSpec::Nth(1));
        run.drop_ns(&mut admin, b"r00").expect("dropped");
        let point = CallResult::Injected(inf_log::fault::TIER_UNLINK_FAIL);
        let failed = await_entry(&mut run, from, "the injected unlink failure", |entry| {
            entry.call == DiskCall::UnlinkTier && entry.result == point
        });
        inf_foundation::fault::disarm_all();
        run.step().expect("the step that consumes the Retry");
        let timers = run.node.cell_loop(0).timer_set();
        let (phase, due) = run
            .node
            .plane(0)
            .tier_retire_role_for_sim(timers)
            .expect("TF1-ROLE: the durable cell holds no retirement role");
        let deadline = timers.next_deadline();
        assert_eq!(
            phase,
            OwnerPhase::Armed,
            "TF1-ROLE: the consumed Retry left the role {phase:?}, due {due:?}"
        );
        let deadline = deadline.expect("TF1-ROLE: no deadline armed");
        assert!(
            due <= deadline && deadline.0 <= due.0 + 1_000_000,
            "TF1-ROLE: the role's deadline {deadline:?} is not within 1 ms after its due {due:?}"
        );
        let removed = await_entry(&mut run, from, "the retry's removal", |entry| {
            entry.call == DiskCall::UnlinkTier
                && entry.path == failed.path
                && entry.result == CallResult::Ok
        });
        let at = removed.mark.map(|mark| mark.at).expect("a marked unlink");
        assert!(at >= due, "TF1-ROLE: the retry's unlink at {at:?}, before its due {due:?}");
        let breaches = run.report.violations.len();
        assert_eq!(breaches, 0, "the run's own oracles: {:?}", run.report.violations);
    }
}
