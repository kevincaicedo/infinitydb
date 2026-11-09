//! The `m4-cold` legs beside `storm`: fixed shapes over tier files of known
//! contents, driven through the real `ColdReads` over the simulated driver
//! and disk, one host turn a round with two pumps, as the storm runs. The
//! harness holds each request's wait itself and polls it once its consumer
//! is due, so it counts the values it received a round independently of the
//! host, and checks every value it received byte for byte against its
//! file. A leg whose shape did not engage reads `VACUOUS`, never green.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::task::{Context, Poll, Waker};

use inf_alloc::{AlignedPool, BufferPool};
use inf_foundation::hash64;
use inf_log::TierIoMode;
use inf_log::fs::sim::{DeviceFault, SimDisk, SimFile};
use inf_log::fs::{SegmentFile, SegmentFs};
use inf_runtime::limits::COLD_DELIVERIES_PER_TURN;
use inf_runtime::{
    BackendDriver, ColdDone, ColdReadConfig, ColdReads, ColdReceipt, ColdRefused, ColdTurn,
    ColdWait, PairIssuer, RawFd, ReadClass, TierFileId, Wait,
};

use super::{ColdLeg, ColdStormReport, ColdStormScenario, RoundCounts, record_receipt};
use crate::net::{CellNet, Plant, SimDriver};

/// The legs' device depth: the default `COLD-READ-QD`, so the pool holds 64
/// buffers and each class queue 256 requests.
const DEPTH: usize = 64;
/// A class queue's cap at that depth.
const CLASS_QUEUE: usize = 4 * DEPTH;
/// One pool buffer, the server's cold window: four tier frames.
const BUFFER_BYTES: usize = 4 * inf_log::TIER_FRAME_BYTES;
/// Where the fixture files live on the sim disk.
const LEG_DIR: &str = "legs/cold";
/// Rounds with no traffic after a leg's last arrival, in which the host
/// drains what is left.
const QUIET_ROUNDS: u64 = 8;
/// The most further quiet rounds a leg waits for its ready delivery, at a
/// turn's allowance a round: every buffer's window at a class queue's
/// members, the most a host at the legs' depth can hold ready.
const DELIVERY_ROUNDS_MAX: u64 = (DEPTH * CLASS_QUEUE) as u64 / COLD_DELIVERIES_PER_TURN as u64;
/// Injected time per round (µs), as the storm's.
const ROUND_US: u64 = 1000;
/// A dense cohort: 31 adjacent requests of 512 B, one buffer's union.
const COHORT: usize = 31;
const COHORT_READ_BYTES: usize = 512;

/// One fixture file: its identity, its descriptor and its open handle,
/// which the sim disk needs alive for reads on the descriptor.
struct LegFile {
    id: TierFileId,
    fd: RawFd,
    path: PathBuf,
    _handle: SimFile,
}

/// One request: its file (an index into the fixture's files), its window,
/// its class, the round from which its consumer polls, and how many rounds
/// it keeps the value after that.
#[derive(Clone, Copy)]
struct Request {
    file: usize,
    offset: u64,
    len: usize,
    class: ReadClass,
    poll_round: u64,
    keep_rounds: u64,
}

/// A foreground request on `file` whose consumer polls at once and drops
/// its value at once.
fn at(file: usize, offset: u64, len: usize) -> Request {
    Request { file, offset, len, class: ReadClass::Foreground, poll_round: 0, keep_rounds: 0 }
}

/// A request the harness holds: its wait until its value is received,
/// then the value until the consumer's round to drop it.
struct Held {
    request: Request,
    wait: Option<ColdWait>,
    value: Option<(ColdDone, u64)>,
}

/// The byte at `offset` of fixture file `file`: the file is the sequence
/// of little-endian words `file << 40 | word index`, so every 8-byte span
/// of every file differs and a value read from another window, file or
/// buffer reads wrong bytes.
fn expected_byte(file: u32, offset: u64) -> u8 {
    let word = (u64::from(file) << 40) | (offset / 8);
    word.to_le_bytes()[usize::try_from(offset % 8).expect("a byte of a word")]
}

/// A leg's world: the host over the sim driver and disk, its files, the
/// requests the harness holds, and the round's counts.
struct Fixture {
    cold: ColdReads,
    driver: SimDriver,
    disk: SimDisk,
    recv_pool: BufferPool,
    files: Vec<LegFile>,
    held: Vec<Held>,
    retry: Vec<Request>,
    round: u64,
    now_us: u64,
    counts: RoundCounts,
    /// Errored values are expected (the `errors` leg): others are a defect.
    errors_expected: bool,
    /// This round's pumps drop every held wait after the drain, before the
    /// receipts are routed.
    cancel_after_issue: bool,
    /// This round's pumps leave the receipts in the device.
    receipts_held: bool,
    /// Requests the host refused for a spent identity space: never offered
    /// again, the refusal being permanent.
    identity_refused: u64,
    report: ColdStormReport,
}

impl Fixture {
    fn new(scenario: &ColdStormScenario) -> Fixture {
        let config =
            ColdReadConfig { qd_cap: DEPTH, overflow_cap: CLASS_QUEUE, ..Default::default() };
        let disk = SimDisk::new();
        disk.create_dir_all(Path::new(LEG_DIR)).expect("the legs' directory");
        let net = CellNet::new(0, scenario.seed ^ 0xD15C, Plant::None);
        // The identity leg places its issuer where `--cold-identity-start`
        // says; every other leg's starts at pair 0.
        let issuer = scenario
            .identity_start
            .map_or(Some(PairIssuer::new()), PairIssuer::starting_at)
            .expect("the identity start is a pair position the command line checked");
        let pool = AlignedPool::new(DEPTH, BUFFER_BYTES);
        Fixture {
            cold: ColdReads::with_issuer(pool, config, issuer),
            driver: SimDriver::with_disk(net, disk.clone()),
            disk,
            recv_pool: BufferPool::new(2, 4096),
            files: Vec::new(),
            held: Vec::new(),
            retry: Vec::new(),
            round: 0,
            now_us: 0,
            counts: RoundCounts::default(),
            errors_expected: false,
            cancel_after_issue: false,
            receipts_held: false,
            identity_refused: 0,
            report: ColdStormReport { leg: scenario.leg, ..ColdStormReport::default() },
        }
    }

    /// Writes `count` fixture files of `bytes` each; returns the first's
    /// index. File ids start at a seed-derived base, so seeds differ.
    fn add_files(&mut self, seed: u64, count: usize, bytes: usize) -> usize {
        let first = self.files.len();
        let base = u32::try_from(seed % 1024).expect("below 1024") * 1024;
        for index in first..first + count {
            let id = base + u32::try_from(index).expect("fixture file");
            let path = Path::new(LEG_DIR).join(format!("{id:08}.tier"));
            let mut handle = self.disk.create_tier(&path, TierIoMode::Buffered).expect("sim file");
            let contents: Vec<u8> = (0..bytes as u64).map(|at| expected_byte(id, at)).collect();
            handle.write_at(0, &contents).expect("sim write");
            let fd = handle.raw_fd().expect("sim files carry fake fds");
            self.files.push(LegFile { id: TierFileId::new(id), fd, path, _handle: handle });
        }
        first
    }

    /// Offers `request`: held when admitted, kept for the next round when
    /// its class queue is full (the storm's `Plan::Dry`).
    fn offer(&mut self, request: Request) {
        let file = &self.files[request.file];
        let asked = self.cold.enqueue(
            file.fd,
            file.id,
            request.offset,
            request.len,
            request.class,
            self.now_us,
        );
        match asked {
            Ok(wait) => self.held.push(Held { request, wait: Some(wait), value: None }),
            Err(ColdRefused::QueueFull) => self.retry.push(request),
            Err(ColdRefused::Unrepresentable(read)) => {
                self.report
                    .violations
                    .push(format!("a leg's request is unrepresentable: {read:?}"));
            }
            Err(ColdRefused::IdentityExhausted) => self.identity_refused += 1,
        }
    }

    /// One pump: the drain, then the device's receipts recorded and the
    /// turn's delivery served, unless the round holds the receipts. Under
    /// `cancel_after_issue` the receipts wait in the device until no request
    /// is queued, and then every held wait is dropped before they are
    /// recorded.
    fn pump(&mut self, turn: &mut ColdTurn) {
        let cold = self.cold.clone();
        let driver = &mut self.driver;
        let issued = cold.drain(|op| driver.push(op));
        self.counts.issued += issued;
        self.counts.issued_drain_max = self.counts.issued_drain_max.max(issued);
        if self.cancel_after_issue {
            if cold.queue_depth() > 0 {
                return;
            }
            for held in &mut self.held {
                held.wait = None;
            }
        }
        if self.receipts_held {
            return;
        }
        let mut out = Vec::new();
        self.driver.submit_and_reap(&mut self.recv_pool, Wait::Poll, &mut out).expect("sim submit");
        for completion in out {
            if let ColdReceipt::Foreign = record_receipt(&cold, completion, self.now_us) {
                self.report.violations.push("a cold receipt named no issued read".into());
            }
        }
        self.deliver(turn);
    }

    /// The turn's delivery service (the plane's `before_execute` role): the
    /// values delivered, counted.
    fn deliver(&mut self, turn: &mut ColdTurn) {
        let progress = self.cold.deliver_ready(turn.delivery(), self.now_us);
        self.counts.delivered += progress.delivered;
    }

    /// The consumers: each due wait polled once (a value received is
    /// counted and checked), each value past its keep dropped.
    fn consume(&mut self) {
        let mut cx = Context::from_waker(Waker::noop());
        let round = self.round;
        let mut received = Vec::new();
        for (index, held) in self.held.iter_mut().enumerate() {
            if held.request.poll_round > round {
                continue;
            }
            let Some(wait) = held.wait.as_mut() else { continue };
            if let Poll::Ready(done) = core::pin::Pin::new(wait).poll(&mut cx) {
                held.wait = None;
                received.push(index);
                held.value = Some((done, round + held.request.keep_rounds));
            }
        }
        for index in received {
            self.counts.received += 1;
            self.check(index);
        }
        for held in &mut self.held {
            if held.value.as_ref().is_some_and(|(_, until)| *until <= round) {
                held.value = None;
            }
        }
        self.held.retain(|held| held.wait.is_some() || held.value.is_some());
    }

    /// Checks one received value against its file's bytes.
    fn check(&mut self, index: usize) {
        let held = &self.held[index];
        let request = held.request;
        let Some((done, _)) = held.value.as_ref() else { return };
        let file = self.files[request.file].id.as_u32();
        let wrong = match done.outcome() {
            Ok(()) => done.bytes(|bytes| {
                bytes.len() != request.len
                    || (0u64..)
                        .zip(bytes)
                        .any(|(at, &byte)| byte != expected_byte(file, request.offset + at))
            }),
            Err(errno) if self.errors_expected => {
                self.report.errored_values += 1;
                self.report.trace_hash = hash64(&errno.to_le_bytes(), self.report.trace_hash);
                false
            }
            Err(_) => true,
        };
        if wrong {
            self.report.violations.push(format!(
                "round {}: the value of file {file} [{}, +{}) is not its file's bytes ({:?})",
                self.round,
                request.offset,
                request.len,
                done.outcome()
            ));
        }
    }

    /// One round, one host turn: the turn's delivery serves the ready work,
    /// then the arrivals and the requests refused last round are offered,
    /// then two pumps, each followed by the consumers, share the turn's
    /// allowance (ADR-0181 R2).
    fn round(&mut self, arrivals: impl IntoIterator<Item = Request>) {
        self.round += 1;
        self.now_us = self.round * ROUND_US;
        self.counts = RoundCounts::default();
        let unclaimed = self.cold.counters().unclaimed;
        let mut turn = ColdTurn::begin();
        self.deliver(&mut turn);
        let retry = std::mem::take(&mut self.retry);
        for request in retry.into_iter().chain(arrivals) {
            self.offer(request);
        }
        for _ in 0..2 {
            self.pump(&mut turn);
            self.consume();
        }
        self.counts.orphaned =
            u32::try_from(self.cold.counters().unclaimed - unclaimed).expect("a round's members");
        let counts = self.counts;
        self.report.trace_hash =
            hash64(&counts.fold().to_le_bytes(), self.report.trace_hash ^ self.round);
        self.report.rounds.push(counts);
    }

    /// The quiet rounds: no traffic, the host drains what is left, then as
    /// many more as its ready delivery needs, at most
    /// [`DELIVERY_ROUNDS_MAX`].
    fn quiet(&mut self) {
        for _ in 0..QUIET_ROUNDS {
            self.round(None);
        }
        let mut more = 0;
        while self.cold.has_ready_delivery() && more < DELIVERY_ROUNDS_MAX {
            self.round(None);
            more += 1;
        }
        if self.cold.has_ready_delivery() {
            self.report
                .violations
                .push(format!("ready delivery left after {more} further quiet rounds (liveness)"));
        }
    }

    #[track_caller]
    fn vacuous(&mut self, why: String) {
        self.report.violations.push(format!("VACUOUS {}: {why}", self.report.leg.name()));
    }

    /// The end of every leg: custody reconciles, the admission identity
    /// holds, every request was answered, and the hashes fold the host's
    /// counters and the disk.
    fn finish(mut self) -> ColdStormReport {
        self.receipts_held = false;
        self.cancel_after_issue = false;
        self.quiet();
        if !self.held.is_empty() || !self.retry.is_empty() {
            let (held, retry) = (self.held.len(), self.retry.len());
            self.report
                .violations
                .push(format!("{held} request(s) unanswered and {retry} unoffered"));
        }
        self.held.clear();
        if let Err(leak) = self.cold.reconcile() {
            self.report.violations.push(format!("custody leak: {leak:?}"));
        }
        let counts = self.cold.counters();
        if counts.enqueued != counts.issued + counts.merged_waiters + counts.cancelled_queued {
            self.report.violations.push(format!("admission identity broken: {counts:?}"));
        }
        self.report.merged_waiters = counts.merged_waiters;
        self.report.queue_high_water = counts.queue_depth_high_water;
        self.report.trace_hash =
            hash64(&counts.issued.to_le_bytes(), self.report.trace_hash ^ counts.unclaimed);
        let mut report = self.report;
        report.state.number(b"finish-time-us", self.now_us);
        report.state.disk(&self.disk);
        report.state_hash = report.state.value();
        report
    }
}

/// Runs one non-storm leg. Deterministic from the seed (L7).
pub(super) fn run_leg(scenario: &ColdStormScenario) -> ColdStormReport {
    let mut fx = Fixture::new(scenario);
    match scenario.leg {
        ColdLeg::Storm => unreachable!("the storm runs its own world"),
        ColdLeg::CancelAll => cancel_all(&mut fx, scenario.seed),
        ColdLeg::Held => held(&mut fx, scenario.seed),
        ColdLeg::DeepQueue => deep_queue(&mut fx, scenario.seed),
        ColdLeg::Stride => stride(&mut fx, scenario.seed),
        ColdLeg::Dense => dense(&mut fx, scenario.seed),
        ColdLeg::Errors => errors(&mut fx, scenario.seed),
        ColdLeg::Identity => identity(&mut fx, scenario),
        ColdLeg::Stop => stop(&mut fx, scenario.seed),
    }
    fx.finish()
}

/// 65 identical requests on one file in one round, every wait dropped
/// after the drain and before the receipt: every member is orphaned at its
/// delivery, and no task runs and no I/O arrives after.
fn cancel_all(fx: &mut Fixture, seed: u64) {
    const MEMBERS: usize = 65;
    let file = fx.add_files(seed, 1, BUFFER_BYTES);
    fx.cancel_after_issue = true;
    fx.round((0..MEMBERS).map(|_| at(file, 0, 64)));
    fx.cancel_after_issue = false;
    // A turn visits at most its allowance: the last member is the next
    // turn's.
    fx.round(None);
    let counts = fx.cold.counters();
    if counts.unclaimed != MEMBERS as u64 || counts.delivered != 0 {
        fx.vacuous(format!("{MEMBERS} members orphaned wanted, {counts:?}"));
    }
}

/// 128 requests on 128 files, over the pool's 64 buffers, whose consumers
/// poll two rounds late: every buffer is leased before a value drops, so a
/// drain finds the pool dry; then no traffic.
fn held(fx: &mut Fixture, seed: u64) {
    const REQUESTS: usize = 2 * DEPTH;
    let first = fx.add_files(seed, REQUESTS, BUFFER_BYTES);
    let late = |file: usize| Request { poll_round: 3, ..at(first + file, 0, 4096) };
    let arrivals: Vec<Request> = (0..REQUESTS).map(late).collect();
    fx.round(arrivals);
    if fx.cold.counters().pool_dry == 0 {
        fx.vacuous(
            "no drain found the pool dry: a value dropped before every buffer was leased".into(),
        );
    }
}

/// 257 requests on 257 files offered in one round before its drain: 256
/// queue, the 257th meets the full class queue and is offered again in the
/// next round.
fn deep_queue(fx: &mut Fixture, seed: u64) {
    let first = fx.add_files(seed, CLASS_QUEUE + 1, BUFFER_BYTES);
    let mut arrivals: Vec<Request> =
        (0..=CLASS_QUEUE).map(|file| at(first + file, 0, 64)).collect();
    arrivals.rotate_left(usize::try_from(seed % 257).expect("below 257"));
    fx.round(arrivals);
    let counts = fx.cold.counters();
    if counts.queue_full == 0 || counts.queue_depth_high_water != CLASS_QUEUE as u32 {
        fx.vacuous(format!("the class queue never filled and refused: {counts:?}"));
    }
}

/// A class queue's 256 requests on one file at a stride of one buffer plus
/// one byte, merge on: no two can join.
fn stride(fx: &mut Fixture, seed: u64) {
    const STRIDE: u64 = BUFFER_BYTES as u64 + 1;
    let bytes = CLASS_QUEUE * (BUFFER_BYTES + 1);
    let file = fx.add_files(seed, 1, bytes);
    let arrivals: Vec<Request> =
        (0..CLASS_QUEUE as u64).map(|index| at(file, index * STRIDE, 4096)).collect();
    fx.round(arrivals);
    let counts = fx.cold.counters();
    if counts.merged_waiters != 0 || counts.queue_depth_high_water != CLASS_QUEUE as u32 {
        fx.vacuous(format!("the stride joined requests or never filled the queue: {counts:?}"));
    }
}

/// Eight rounds of eight 31-member cohorts, each 31 adjacent requests on
/// its own file, a buffer's union: 248 requests a round, one class queue.
fn dense(fx: &mut Fixture, seed: u64) {
    const ROUNDS: usize = 8;
    const COHORTS: usize = 8;
    let first = fx.add_files(seed, ROUNDS * COHORTS, BUFFER_BYTES);
    for round in 0..ROUNDS {
        let mut cohorts: Vec<usize> =
            (0..COHORTS).map(|cohort| first + round * COHORTS + cohort).collect();
        cohorts.rotate_left(usize::try_from(seed % COHORTS as u64).expect("below 8"));
        let member = |file: usize, index: usize| {
            at(file, (index * COHORT_READ_BYTES) as u64, COHORT_READ_BYTES)
        };
        let members =
            cohorts.into_iter().flat_map(|file| (0..COHORT).map(move |index| (file, index)));
        let arrivals: Vec<Request> = members.map(|(file, index)| member(file, index)).collect();
        fx.round(arrivals);
    }
    let cohorts = (ROUNDS * COHORTS) as u64;
    if fx.cold.counters().merged_waiters < cohorts * (COHORT as u64 - 1) {
        fx.vacuous(format!("{cohorts} cohorts of {COHORT} did not merge whole"));
    }
}

/// Eight rounds of eight singletons and one eight-member cohort, the first
/// read of every cohort's file and of every other singleton's failing
/// `EIO` (the sim disk's read fault): each member of an errored read
/// carries the error, custody as on success.
fn errors(fx: &mut Fixture, seed: u64) {
    const ROUNDS: usize = 8;
    const SINGLES: usize = 8;
    const MEMBERS: usize = 8;
    fx.errors_expected = true;
    let first = fx.add_files(seed, ROUNDS * (SINGLES + 1), BUFFER_BYTES);
    // Every other singleton's file and every cohort's: errored and
    // successful reads in each round.
    for (index, file) in fx.files[first..].iter().enumerate() {
        if index % 2 == 0 || index % (SINGLES + 1) == SINGLES {
            fx.disk.inject(&file.path, DeviceFault::ReadEio, 1).expect("the read fault arms");
        }
    }
    for round in 0..ROUNDS {
        let base = first + round * (SINGLES + 1);
        let singles = (0..SINGLES).map(|file| at(base + file, 0, 64));
        let cohort = (0..MEMBERS).map(|member| at(base + SINGLES, (member * 64) as u64, 64));
        let arrivals: Vec<Request> = singles.chain(cohort).collect();
        fx.round(arrivals);
    }
    if fx.cold.counters().errored == 0 || fx.report.errored_values == 0 {
        fx.vacuous("no read failed: the sim disk's read fault never fired".into());
    }
}

/// Reads across the point where the issuer's identities run out, set by
/// `--cold-identity-start`, the first read held to the end: once the space is
/// spent every request is refused `IdentityExhausted`, for good, and every
/// request admitted before is answered (custody and answers are `finish`'s).
/// A run whose requests all fit the space left reads `VACUOUS`.
fn identity(fx: &mut Fixture, scenario: &ColdStormScenario) {
    const REQUESTS: usize = DEPTH;
    let start = scenario.identity_start.expect("the identity leg names its start");
    let first = fx.add_files(scenario.seed, REQUESTS, BUFFER_BYTES);
    let kept = Request { keep_rounds: QUIET_ROUNDS, ..at(first, 0, 64) };
    fx.round([kept]);
    fx.round((1..REQUESTS).map(|file| at(first + file, 0, 64)));
    fx.report.trace_hash = hash64(&start.to_le_bytes(), fx.report.trace_hash);
    let left = inf_runtime::limits::COLD_IDENTITY_PAIRS_MAX - start;
    let refused = fx.identity_refused;
    let admitted = REQUESTS as u64 - refused;
    if refused == 0 {
        fx.vacuous(format!(
            "{REQUESTS} requests fit the {left} pair(s) left at {start:#x}: no identity ran out"
        ));
    } else if fx.cold.identity_refusals() != refused || admitted != left.min(REQUESTS as u64) {
        fx.report.violations.push(format!(
            "identity: {refused} refusal(s) seen, the host counted {}; {admitted} admitted \
             where {left} pair(s) were left",
            fx.cold.identity_refusals()
        ));
    }
}

/// A stop's shape: 100 values held over four windows; then 60 singletons
/// and 128 requests of each class offered, so the pool's last 60 buffers go
/// in flight (their receipts left in the device) and 256 requests stay
/// queued; then the barrier, after which one maintain and one foreground
/// request are offered. The host here has no stop to mark at
/// the barrier, so both are admitted and the leg reads `VACUOUS`.
fn stop(fx: &mut Fixture, seed: u64) {
    const WINDOWS: usize = 4;
    const MEMBERS: usize = 25;
    const IN_FLIGHT: usize = DEPTH - WINDOWS;
    const QUEUED: usize = CLASS_QUEUE / 2;
    let first = fx.add_files(seed, WINDOWS + IN_FLIGHT + 2 * QUEUED + 2, BUFFER_BYTES);
    let keep = |request: Request| Request { keep_rounds: 4, ..request };
    let windows = (0..WINDOWS).flat_map(|w| (0..MEMBERS).map(move |m| (w, m)));
    let arrivals: Vec<Request> =
        windows.map(|(w, m)| keep(at(first + w, (m * 64) as u64, 64))).collect();
    fx.round(arrivals);
    fx.receipts_held = true;
    let base = first + WINDOWS;
    let flight = (0..IN_FLIGHT).map(|file| at(base + file, 0, 64));
    let class =
        |index: usize| if index < QUEUED { ReadClass::Foreground } else { ReadClass::Maintain };
    let queued = (0..2 * QUEUED)
        .map(|index| Request { class: class(index), ..at(base + IN_FLIGHT + index, 0, 64) });
    let arrivals: Vec<Request> = flight.chain(queued).collect();
    fx.round(arrivals);
    let barrier = fx.round;
    fx.receipts_held = false;
    let last = base + IN_FLIGHT + 2 * QUEUED;
    let before = fx.held.len();
    fx.offer(Request { class: ReadClass::Maintain, ..at(last, 0, 64) });
    fx.offer(at(last + 1, 0, 64));
    let admitted = fx.held.len() - before;
    fx.vacuous(format!(
        "the host has no stop to mark at the barrier (round {barrier}); {admitted} of the 2 \
         requests offered after it were admitted"
    ));
}
