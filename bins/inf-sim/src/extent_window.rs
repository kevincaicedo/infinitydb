//! `m4-extent-window` (ADR-0163 D2; ADR-0061 D3): the extent-seal window's
//! steel-thread row. A blob `SET` writes its extent file and holds it open
//! until the extent's fdatasync folds into the cell's fsync ledger; the
//! window bounds those barriers at 16 per cell, so at most 16 extent files
//! are open and the rest of a burst waits, parked, with no file created.
//!
//! One cell, one tiered `always` namespace, closed-loop writers of 4 KiB
//! blob values over a device whose fdatasync takes about two reactor
//! iterations, on the flush unit's one timeline. Every connection's
//! pending bytes are delivered whole, so the writers' commands, sent at
//! one step, arrive at one reactor iteration: the burst the window paces.
//!
//! Oracles, each read from what the window does not produce:
//!
//! - the sim disk's peak of open extent descriptors is at most the cap
//!   (token `extent-handles-over`);
//! - the extent files the sim disk saw created are at most the blob writes
//!   acknowledged, and the writes of the clients that left: a parked writer
//!   creates none (`extent-files-over-writes`);
//! - every reply is the exact `+OK` (`extent-reply-wrong`);
//! - every writer whose extent file was created at the first step that
//!   created one has its reply by the step that creates the 33rd file: a
//!   frame held for extent barriers is held for one turnover, not for the
//!   whole queue (`extent-hold-overran`, the window's H3);
//! - no writer is left waiting: a run that stops making progress with a
//!   reply owed, or ends with a writer parked or a turn out, is red
//!   (`writer-left-parked`). On the burst two clients disconnect with a
//!   write in flight, one parked behind the first window and one at the
//!   first handout: a disconnect does not cancel a command already parsed
//!   (the plane runs it to completion, its reply dropped), so their writes
//!   take their turns like any other and the rest are not held behind
//!   them.
//!
//! And the product's own counts, read as wiring checks with their
//! engagement: the window's census row held its cap, with a crossing on the
//! burst and none at exactly the cap (H2; `cap-row-unfilled`,
//! `cap-row-crossed`), and at least three frames were held for extent
//! barriers (H3's engagement, `extent-hold-unengaged`). The cap bound, the
//! assembly rule, the turn rule (H4) and their liveness are the census
//! oracle's, read at the node's drop. A violation's token is whitespace-free
//! and carried by that violation's text alone, so a canary row can expect it.
//!
//! The cold leg ([`cold`]) paces writers that overwrite cold keys, whose
//! every resolve is a cold read: there a reply that is `BUSY` is red
//! (`extent-reply-busy`).

mod cold;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use inf_foundation::bounded::CapRow;
use inf_foundation::hash64;
use inf_foundation::rng::SplitMix64;
use inf_foundation::time::{Clock, Nanos, VirtualClock};
use inf_log::fs::sim::SimDisk;
use inf_server::StallConfig;

use crate::durable::{
    DurableScenario, DurableWorkload, MiniClient, Node, STALL_STEPS, TraceObserver, boot,
    build_disk, encode,
};
use crate::net::Plant;
use crate::resp::reply_len;

/// The tiered namespace every writer uses.
const NS_NAME: &[u8] = b"ew";
/// The window's cap, as the oracles read it: unfolded extent barriers per
/// cell.
const WINDOW_ENTRIES: usize = 16;
/// The frame holds H3 needs to be engaged: three turnovers.
const H3_HOLDS_MIN: u64 = 3;
/// The file count by which H3 requires the first cohort's replies: two
/// windows' worth of files, so a correct build has sealed the first
/// cohort's frame and acknowledged it before the third window registers.
const H3_FILES: usize = 2 * WINDOW_ENTRIES + 1;
/// A blob value: at the namespace's 4 KiB `BLOB-THRESHOLD`.
const VALUE_LEN: usize = 4 << 10;
/// The extent header block: a value's first byte lies at this file offset.
const EXTENT_HEADER_BYTES: usize = 4096;
/// The digits of a writer's id at the head of each of its values, so an
/// extent file names its writer.
const ID_DIGITS: usize = 8;
/// Max virtual nanoseconds per scheduler step: about 1 ms a step.
const STEP_NS_MAX: u64 = 2_000_000;
/// The fdatasync service time: two steps' mean, on the flush unit's one
/// timeline, so the window's 16 barriers drain in about 32 iterations.
const SYNC_NS: u64 = STEP_NS_MAX;

/// The scenario's legs, named on the command line (`--window-leg`).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ExtentWindowLeg {
    /// 48 writers, three windows' worth: the first 16 register, the 17th
    /// finds the window full (the row's crossing) and the rest park; the
    /// window opens once per turnover.
    Burst,
    /// 16 writers, exactly the cap: the window fills and never crosses —
    /// the control leg.
    Cap,
    /// 56 writers overwrite 56 cold keys with blob values, so each resolve
    /// is a cold read, against a cold FIFO of 16 places; a 57th writer, on
    /// a key in RAM, arrives while the first handout's turns resolve
    /// ([`cold`]).
    Cold,
}

impl ExtentWindowLeg {
    pub const ALL: [ExtentWindowLeg; 3] =
        [ExtentWindowLeg::Burst, ExtentWindowLeg::Cap, ExtentWindowLeg::Cold];

    /// The leg's name on the command line.
    pub fn name(self) -> &'static str {
        match self {
            ExtentWindowLeg::Burst => "burst",
            ExtentWindowLeg::Cap => "cap",
            ExtentWindowLeg::Cold => "cold",
        }
    }

    /// The leg named `name`, if any.
    pub fn parse(name: &str) -> Option<ExtentWindowLeg> {
        ExtentWindowLeg::ALL.into_iter().find(|leg| leg.name() == name)
    }

    fn writers(self) -> usize {
        match self {
            ExtentWindowLeg::Burst => 3 * WINDOW_ENTRIES,
            ExtentWindowLeg::Cap => WINDOW_ENTRIES,
            ExtentWindowLeg::Cold => cold::WRITERS,
        }
    }

    /// Blob `SET`s per writer: enough files for H3 on the burst and cap
    /// legs; one overwrite of its key on the cold leg.
    fn ops_per_writer(self) -> u64 {
        match self {
            ExtentWindowLeg::Burst => 3,
            ExtentWindowLeg::Cap => 4,
            ExtentWindowLeg::Cold => 1,
        }
    }

    /// Max virtual nanoseconds per scheduler step.
    fn step_ns_max(self) -> u64 {
        match self {
            ExtentWindowLeg::Burst | ExtentWindowLeg::Cap => STEP_NS_MAX,
            ExtentWindowLeg::Cold => cold::STEP_NS_MAX,
        }
    }

    /// The device: the fdatasync on the flush unit's one timeline, plain
    /// writes off it at a page copy's 8 µs (so the boot's write-reorder
    /// window is open); on the cold leg its tier reads on the byte
    /// timeline too.
    fn stall(self) -> StallConfig {
        match self {
            ExtentWindowLeg::Burst | ExtentWindowLeg::Cap => {
                StallConfig { base_ns: SYNC_NS, write_base_ns: 8_000, ..StallConfig::instant() }
            }
            ExtentWindowLeg::Cold => cold::stall(),
        }
    }
}

/// One seeded run of one leg.
#[derive(Clone, Debug)]
pub struct ExtentWindowScenario {
    pub seed: u64,
    pub leg: ExtentWindowLeg,
}

/// What one run observed: the disk's own counts beside the violations.
#[derive(Clone, Debug, Default)]
pub struct ExtentWindowReport {
    pub trace: Vec<u8>,
    pub trace_hash: u64,
    pub state_hash: u64,
    pub violations: Vec<String>,
    pub scheduler_steps: u64,
    /// The most extent files open at once, by the sim disk's descriptors.
    pub handles_peak: usize,
    /// Extent files the sim disk saw created.
    pub files_created: usize,
    /// Blob `SET`s acknowledged `+OK`.
    pub sets_acked: u64,
    /// Clients that disconnected with a write in flight.
    pub writers_left: u64,
    /// Rows the census oracle read (node sum).
    pub cap_rows_checked: u64,
    /// The cell's `extent-window` census row at the end of the traffic.
    pub window_row: Option<CapRow>,
    /// The cell's durable counters at the end of the traffic: frames held
    /// for extent barriers (episodes), park episodes, and the writers
    /// parked and turns out then.
    pub frame_waits_extent: u64,
    pub extent_parks: u64,
    pub extent_parked: u64,
    pub extent_turns_out: u64,
}

impl ExtentWindowReport {
    #[must_use]
    pub fn ok(&self) -> bool {
        self.violations.is_empty()
    }
}

/// One closed-loop writer: its connection, its sends and its replies.
struct Writer {
    id: usize,
    fd: std::os::fd::RawFd,
    rx: Vec<u8>,
    /// Blob `SET`s sent; the setup `INF.NS USE` is not one.
    sent: u64,
    quota: u64,
    /// Replies read to its `SET`s, and the step of each.
    replied_at: Vec<u64>,
    setup_done: bool,
    inflight: bool,
    /// The one key it overwrites (the cold leg); `None`: a fresh key a
    /// write, `k:<id>:<n>`.
    key: Option<Vec<u8>>,
    /// It sends nothing before this instant (the cold leg's pacing).
    not_before: Nanos,
    /// The client closed its connection with a write in flight: no reply
    /// is read, nothing more is sent.
    left: bool,
}

impl Writer {
    fn new(node: &mut Node, id: usize, quota: u64) -> Writer {
        let fd = node.nets[0].borrow_mut().connect();
        node.nets[0].borrow_mut().client_send(fd, &encode(&[b"INF.NS", b"USE", NS_NAME]));
        Writer {
            id,
            fd,
            rx: Vec::new(),
            sent: 0,
            quota,
            replied_at: Vec::new(),
            setup_done: false,
            inflight: true,
            key: None,
            not_before: Nanos(0),
            left: false,
        }
    }

    fn value(&self) -> Vec<u8> {
        let mut value = format!("{:0width$}", self.id, width = ID_DIGITS).into_bytes();
        value.resize(VALUE_LEN, b'v');
        value
    }

    fn next_key(&self) -> Vec<u8> {
        self.key.clone().unwrap_or_else(|| format!("k:{}:{}", self.id, self.sent).into_bytes())
    }

    fn done(&self) -> bool {
        self.left || (self.setup_done && !self.inflight && self.sent >= self.quota)
    }

    /// The client disconnects, its write in flight.
    fn leave(&mut self, node: &mut Node) {
        node.nets[0].borrow_mut().client_close(self.fd);
        self.left = true;
    }
}

/// Whether `name` is an extent file (`blob-NNNNNN.iblob`).
fn is_extent(name: &Path) -> bool {
    name.file_name()
        .and_then(|n| n.to_str())
        .and_then(inf_log::blob::parse_extent_file_name)
        .is_some()
}

/// The writer an extent file names: the id at the head of its value.
fn writer_of(disk: &SimDisk, path: &Path) -> Option<usize> {
    let bytes = disk.contents(path)?;
    let head = bytes.get(EXTENT_HEADER_BYTES..EXTENT_HEADER_BYTES + ID_DIGITS)?;
    std::str::from_utf8(head).ok()?.parse().ok()
}

/// The disk-side record of extent creations, step by step.
#[derive(Default)]
struct Creations {
    seen: BTreeSet<PathBuf>,
    /// `(step, writer)` per creation, in creation order (one step's files in
    /// name order).
    log: Vec<(u64, Option<usize>)>,
    handles_peak: usize,
}

impl Creations {
    fn sample(&mut self, disk: &SimDisk, step: u64) {
        self.handles_peak = self.handles_peak.max(disk.open_files_named(is_extent));
        for path in disk.names_where(is_extent) {
            if self.seen.insert(path.clone()) {
                self.log.push((step, writer_of(disk, &path)));
            }
        }
    }
}

impl ExtentWindowScenario {
    #[must_use]
    pub fn m4_extent_window(seed: u64, leg: ExtentWindowLeg) -> ExtentWindowScenario {
        ExtentWindowScenario { seed, leg }
    }

    /// The harness plumbing view (`boot` and `Node::step` read the cell
    /// count, the seed, the segment and checkpoint shape through it).
    fn harness(&self) -> DurableScenario {
        DurableScenario {
            seed: self.seed,
            workload: DurableWorkload::KeyValue,
            cells: 1,
            always_writers: 0,
            esec_think_ns_max: 0,
            always_think_ns_max: 0,
            esec_writers: 0,
            mem_writers: 0,
            ops_per_writer: 0,
            keys_per_writer: 0,
            value_max: 0,
            value_pad_max: 0,
            step_ns_max: self.leg.step_ns_max(),
            double_cut: false,
            plant: Plant::None,
            segment_bytes: 1 << 20,
            // No automatic checkpoint: the log holds the run.
            ckpt_interval_bytes: 0,
            ckpt_stream_bytes_per_sec: None,
            ckpt_section_bytes: None,
            ckpt_section_bound: None,
            stall: Some(self.leg.stall()),
            replay_canary: false,
            clean_stop: false,
            io_mode: inf_server::SegmentIoMode::Buffered,
            frames_in_flight: 1,
            device: Default::default(),
            budget_oracle: false,
            ckpt_overrun: None,
            reorder_oracle: false,
            ckpt_direct_refused_after: None,
            fill: Default::default(),
            group: Default::default(),
            prelude: None,
            recycle_slots: 0,
            prealloc: inf_server::PreallocPolicy::DEFAULT,
            recycle_oracle: false,
            recycle_open_fault: false,
            lift_regime: false,
            spin_iters: DurableScenario::SPIN_ITERS,
            tier_budget: None,
            epoch_marks: None,
        }
    }
}

/// Runs one seeded leg; every node is dropped before the observer seals,
/// so the census verdict of each cell is in the report.
#[must_use]
pub fn run_extent_window_scenario(scenario: &ExtentWindowScenario) -> ExtentWindowReport {
    let observer = TraceObserver::default();
    let mut report = run_observed(scenario, &observer);
    let sealed = observer.seal();
    report.state_hash = sealed.state_hash;
    report.cap_rows_checked = sealed.census.rows_checked;
    report.violations.extend(sealed.census.violations);
    report.trace = observer.trace_bytes();
    report.trace_hash = hash64(&report.trace, 0xE57E);
    report
}

fn run_observed(scenario: &ExtentWindowScenario, observer: &TraceObserver) -> ExtentWindowReport {
    let harness = scenario.harness();
    let clock = Rc::new(VirtualClock::new(Nanos(1)));
    let disk = build_disk(scenario.seed, harness.stall.as_ref());
    let mut rng = SplitMix64::new(scenario.seed ^ 0xE57E_A11D);
    let mut report = ExtentWindowReport::default();
    let mut node = match boot(&harness, PathBuf::from("node"), &disk, &clock, observer) {
        Ok(node) => node,
        Err(err) => {
            report.violations.push(format!("boot failed: {err}"));
            return report;
        }
    };
    node.nets[0].borrow_mut().deliver_whole();
    if let Err(what) = create_namespace(&mut node, &mut rng, &clock, &disk, scenario.leg) {
        report.violations.push(what);
        return report;
    }
    let mut writers: Vec<Writer> = (0..scenario.leg.writers())
        .map(|id| Writer::new(&mut node, id, scenario.leg.ops_per_writer()))
        .collect();
    let mut creations = Creations::default();
    let mut run = Run { scenario, node: &mut node, rng: &mut rng, clock: &clock, disk: &disk };
    match scenario.leg {
        ExtentWindowLeg::Burst | ExtentWindowLeg::Cap => {
            drive(&mut run, &mut writers, &mut creations, &mut report);
        }
        ExtentWindowLeg::Cold => cold::run(&mut run, &mut writers, &mut creations, &mut report),
    }
    report.handles_peak = creations.handles_peak;
    report.files_created = creations.seen.len();
    report.sets_acked = writers.iter().map(|w| w.replied_at.len() as u64).sum();
    scrape(&node, &mut report);
    judge(scenario, &writers, &creations, &mut report);
    drop(node);
    report
}

/// One run's node and its instruments, for the traffic phases.
struct Run<'a> {
    scenario: &'a ExtentWindowScenario,
    node: &'a mut Node,
    rng: &'a mut SplitMix64,
    clock: &'a Rc<VirtualClock>,
    disk: &'a SimDisk,
}

impl Run<'_> {
    /// One scheduler step at the leg's step size.
    fn step(&mut self) -> std::io::Result<()> {
        self.node.step(self.rng, self.clock, self.disk, self.scenario.leg.step_ns_max())
    }
}

/// `INF.NS CREATE` of the tiered `always` namespace: the m4 tiered shape's
/// smallest window (3 MiB, a tenth mutable, 1 MiB slices) and the 4 KiB
/// blob threshold, so every writer's value rides an extent; on the cold
/// leg a `COLD-READ-QD` of 4, so the cell's cold FIFO has 16 places.
fn create_namespace(
    node: &mut Node,
    rng: &mut SplitMix64,
    clock: &Rc<VirtualClock>,
    disk: &SimDisk,
    leg: ExtentWindowLeg,
) -> Result<(), String> {
    let mut setup = MiniClient::connect(node, 0);
    let mut create: Vec<&[u8]> = vec![
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
        b"BLOB-THRESHOLD",
        b"4kb",
        b"TIER-IO-MODE",
        b"buffered",
    ];
    if leg == ExtentWindowLeg::Cold {
        create.extend_from_slice(&[b"COLD-READ-QD", cold::READ_QD]);
    }
    match setup.call(node, rng, clock, disk, leg.step_ns_max(), &create) {
        Ok(Some(ok)) if ok == b"+OK\r\n" => Ok(()),
        Ok(other) => Err(format!("DDL CREATE answered {other:?}")),
        Err(err) => Err(format!("DDL phase: {err}")),
    }
}

/// Steps the node until every writer has its replies, sampling the disk
/// after each step; a run that makes no progress for `STALL_STEPS` steps
/// with a reply owed is red.
fn drive(
    run: &mut Run<'_>,
    writers: &mut [Writer],
    creations: &mut Creations,
    report: &mut ExtentWindowReport,
) {
    let mut idle = 0u64;
    let mut leavers = Leavers::default();
    while writers.iter().any(|w| !w.done()) {
        if !traffic_step(run, writers, creations, report, &mut idle) {
            return;
        }
        if run.scenario.leg == ExtentWindowLeg::Burst {
            leavers.step(run.node, writers, creations);
        }
    }
    report.writers_left = writers.iter().filter(|w| w.left).count() as u64;
}

/// The burst's two disconnects, each with a write in flight: the highest
/// writer outside the first cohort once the cohort's files exist (parked
/// at the queue's tail), and the lowest outside it once the first handout
/// has turns out (at the queue's head).
#[derive(Default)]
struct Leavers {
    parked: Option<usize>,
    at_handout: Option<usize>,
}

impl Leavers {
    fn step(&mut self, node: &mut Node, writers: &mut [Writer], creations: &Creations) {
        let Some(&(first, _)) = creations.log.first() else { return };
        let cohort: BTreeSet<usize> = creations
            .log
            .iter()
            .take_while(|&&(step, _)| step == first)
            .filter_map(|&(_, writer)| writer)
            .collect();
        let candidate = |w: &&mut Writer| !cohort.contains(&w.id) && !w.left && w.inflight;
        if self.parked.is_none() {
            if let Some(writer) = writers.iter_mut().rev().find(candidate) {
                writer.leave(node);
                self.parked = Some(writer.id);
            }
            return;
        }
        let turns_out = node.plane(0).durable_stats().map_or(0, |s| s.extent_turns_out);
        if self.at_handout.is_none()
            && turns_out > 0
            && let Some(writer) = writers.iter_mut().find(candidate)
        {
            writer.leave(node);
            self.at_handout = Some(writer.id);
        }
    }
}

/// One step of the traffic phase: the node steps, the disk is sampled, and
/// each writer reads its replies and sends. Returns `false` once the run
/// must end: a step failed, or no progress for `STALL_STEPS` steps with a
/// reply owed.
fn traffic_step(
    run: &mut Run<'_>,
    writers: &mut [Writer],
    creations: &mut Creations,
    report: &mut ExtentWindowReport,
    idle: &mut u64,
) -> bool {
    report.scheduler_steps += 1;
    let step = report.scheduler_steps;
    if let Err(err) = run.step() {
        report.violations.push(format!("traffic phase: {err}"));
        return false;
    }
    creations.sample(run.disk, step);
    let now = run.clock.now();
    let mut progress = false;
    for writer in writers.iter_mut() {
        progress |= pump(run.scenario.seed, run.node, writer, step, now, report);
    }
    *idle = if progress { 0 } else { *idle + 1 };
    if *idle < STALL_STEPS {
        return true;
    }
    let owed: Vec<String> = writers
        .iter()
        .filter(|w| !w.done())
        .map(|w| format!("writer {} ({} of {} replied)", w.id, w.replied_at.len(), w.quota))
        .collect();
    report.violations.push(format!(
        "EXTENT WINDOW LIVENESS VIOLATION seed {:#x}: writer-left-parked — no progress in \
         {STALL_STEPS} steps with replies owed: {}",
        run.scenario.seed,
        owed.join(", ")
    ));
    false
}

/// One writer's turn after a step: read its replies, then send its next
/// `SET` if it has none in flight and its pacing allows. Returns whether
/// anything moved.
fn pump(
    seed: u64,
    node: &mut Node,
    writer: &mut Writer,
    step: u64,
    now: Nanos,
    report: &mut ExtentWindowReport,
) -> bool {
    if writer.left {
        return false;
    }
    let mut net = node.nets[0].borrow_mut();
    let bytes = net.client_recv(writer.fd);
    let mut progress = !bytes.is_empty();
    writer.rx.extend_from_slice(&bytes);
    while let Some(n) = reply_len(&writer.rx) {
        let reply: Vec<u8> = writer.rx.drain(..n).collect();
        writer.inflight = false;
        if reply != b"+OK\r\n" {
            let token = if reply.starts_with(b"-BUSY") {
                "extent-reply-busy"
            } else {
                "extent-reply-wrong"
            };
            report.violations.push(format!(
                "EXTENT WINDOW REPLY VIOLATION seed {seed:#x}: {token} — writer {} read {:?}",
                writer.id,
                String::from_utf8_lossy(&reply)
            ));
        } else if writer.setup_done {
            writer.replied_at.push(step);
        }
        writer.setup_done = true;
    }
    if writer.setup_done
        && !writer.inflight
        && writer.sent < writer.quota
        && now >= writer.not_before
    {
        let key = writer.next_key();
        net.client_send(writer.fd, &encode(&[b"SET", &key, &writer.value()]));
        writer.sent += 1;
        writer.inflight = true;
        progress = true;
    }
    progress
}

/// The cell's window row and durable counters, at the end of the traffic.
fn scrape(node: &Node, report: &mut ExtentWindowReport) {
    let plane = node.plane(0);
    report.window_row = plane.cap_census().row("extent-window");
    if let Some(stats) = plane.durable_stats() {
        report.frame_waits_extent = stats.frame_waits_extent;
        report.extent_parks = stats.extent_parks;
        report.extent_parked = stats.extent_parked;
        report.extent_turns_out = stats.extent_turns_out;
    }
}

/// The run's verdicts on the disk's own counts and the writers' replies.
fn judge(
    scenario: &ExtentWindowScenario,
    writers: &[Writer],
    creations: &Creations,
    report: &mut ExtentWindowReport,
) {
    let seed = scenario.seed;
    if creations.handles_peak > WINDOW_ENTRIES {
        report.violations.push(format!(
            "EXTENT WINDOW VIOLATION seed {seed:#x}: extent-handles-over — {} extent files open \
             at once, above the window of {WINDOW_ENTRIES}",
            creations.handles_peak
        ));
    }
    if creations.seen.len() as u64 > report.sets_acked + report.writers_left {
        report.violations.push(format!(
            "EXTENT WINDOW VIOLATION seed {seed:#x}: extent-files-over-writes — {} extent files \
             created for {} acknowledged blob writes and {} writes of clients that left",
            creations.seen.len(),
            report.sets_acked,
            report.writers_left
        ));
    }
    if report.extent_parked > 0 || report.extent_turns_out > 0 {
        report.violations.push(format!(
            "EXTENT WINDOW LIVENESS VIOLATION seed {seed:#x}: writer-left-parked — {} writer(s) \
             parked and {} turn(s) out after every reply",
            report.extent_parked, report.extent_turns_out
        ));
    }
    if scenario.leg == ExtentWindowLeg::Burst && report.writers_left < 2 {
        report.violations.push(format!(
            "VACUOUS seed {seed:#x}: extent-leave-unengaged — {} of the burst's 2 clients left \
             with a write in flight",
            report.writers_left
        ));
    }
    // H2 and H3 are the burst's and the cap's: the cold leg's pacing is its
    // own, and its writers overwrite one key each.
    if scenario.leg == ExtentWindowLeg::Cold {
        return;
    }
    hold_oracle(seed, writers, creations, report);
    if report.frame_waits_extent < H3_HOLDS_MIN {
        report.violations.push(format!(
            "VACUOUS seed {seed:#x}: extent-hold-unengaged — {} frame(s) held for extent \
             barriers, fewer than {H3_HOLDS_MIN}",
            report.frame_waits_extent
        ));
    }
    window_row_oracle(scenario, report);
}

/// H2 on the window's census row: it held its cap, and crossed it on the
/// burst (the 17th arrival of one iteration) and never at exactly the cap.
fn window_row_oracle(scenario: &ExtentWindowScenario, report: &mut ExtentWindowReport) {
    let seed = scenario.seed;
    let Some(row) = report.window_row else {
        report.violations.push(format!(
            "VACUOUS seed {seed:#x}: cap-row-unfilled — no `extent-window` row at the end"
        ));
        return;
    };
    let (held, cap, crossings) = (row.high_water, row.entries_max, row.crossings);
    let unfilled = held < cap;
    let what = match scenario.leg {
        ExtentWindowLeg::Burst if unfilled || crossings == 0 => format!(
            "VACUOUS seed {seed:#x}: cap-row-unfilled — the `extent-window` row held {held} of \
             {cap} with {crossings} crossing(s): the burst never filled it"
        ),
        ExtentWindowLeg::Cap if unfilled => format!(
            "VACUOUS seed {seed:#x}: cap-row-unfilled — the `extent-window` row held {held} of \
             {cap}: the writers never filled it"
        ),
        ExtentWindowLeg::Cap if crossings > 0 => format!(
            "EXTENT WINDOW VIOLATION seed {seed:#x}: cap-row-crossed — {crossings} crossing(s) \
             with exactly {cap} writers"
        ),
        ExtentWindowLeg::Burst | ExtentWindowLeg::Cap | ExtentWindowLeg::Cold => return,
    };
    report.violations.push(what);
}

/// H3: each writer whose extent file was created at the first step that
/// created one has its first reply by the step that creates the 33rd file.
fn hold_oracle(
    seed: u64,
    writers: &[Writer],
    creations: &Creations,
    report: &mut ExtentWindowReport,
) {
    let (Some(&(first, _)), Some(&(by, _))) =
        (creations.log.first(), creations.log.get(H3_FILES - 1))
    else {
        report.violations.push(format!(
            "VACUOUS seed {seed:#x}: extent-hold-unreached — {} extent files, fewer than \
             {H3_FILES}",
            creations.log.len()
        ));
        return;
    };
    let cohort: BTreeMap<usize, Option<u64>> = creations
        .log
        .iter()
        .take_while(|&&(step, _)| step == first)
        .filter_map(|&(_, writer)| writer)
        .map(|id| (id, writers.get(id).and_then(|w| w.replied_at.first().copied())))
        .collect();
    let late: Vec<String> = cohort
        .iter()
        .filter(|(_, replied)| replied.is_none_or(|at| at > by))
        .map(|(id, replied)| format!("writer {id} replied at {replied:?}"))
        .collect();
    if !late.is_empty() {
        report.violations.push(format!(
            "EXTENT WINDOW VIOLATION seed {seed:#x}: extent-hold-overran — {} of the {} writers \
             whose files the first step created (step {first}) had no reply by step {by}, which \
             created file {H3_FILES}: {}",
            late.len(),
            cohort.len(),
            late.join(", ")
        ));
    }
}
