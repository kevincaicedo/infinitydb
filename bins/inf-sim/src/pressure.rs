//! `m4-pressure` (M4-S07): the throttled-device backpressure scenario —
//! the milestone's nastiest liveness test (plan §5 S07 guide).
//!
//! A seeded write storm runs against a tiered table whose flush leg is a
//! device model the harness paces in **virtual time** (deterministic —
//! the S11 `TierStore` latency model will replace it; ADR-0053 D6):
//!
//! 1. **Throttled phase** — the device serves ~10% of the storm's write
//!    bandwidth. Writers outrun flush, hit the budget window (ADR-0053
//!    D1/D4), and **suspend on flushed-watermark progress** through the
//!    real `WatermarkGate` + executor machinery. Oracles: RAM residency
//!    ≤ budget + one slice at every round, `tail_alloc_stalls` visible,
//!    zero out-of-memory verdicts, and progress continues (the ADR-0053
//!    D5 deadlock-freedom argument, exercised).
//! 2. **Wedged phase** — the device stops entirely for longer than the
//!    stall timeout: parked writers surface the **typed timeout** (the
//!    ADR-0053 D4 `STALLED` class), RSS stays bounded, the loop keeps
//!    turning.
//! 3. **Recovery phase** — the device returns at full speed: the backlog
//!    drains and every key's final content verifies byte-exact against
//!    the model — cold records read back from the simulated tier files'
//!    actual CRC-checked bytes.
//!
//! Stall latency is measured in virtual nanoseconds per stall (park →
//! wake) and reported; every event folds into `trace_hash` and
//! `--verify-determinism` requires two-run identity (L7).
//!
//! Two legs press two windows. `budget` (the default) is a 256 KiB budget at
//! 4 KiB pages, a 128-page ring, so the table's seal marks bucket at one
//! page. `wide-ring` is a 16 MiB budget, an 8,192-page ring past the reach
//! of the marks at one page, so their stride is two pages and a seal step
//! may overrun its slice by one stride: the storm's values are larger, so
//! that it fills the window, and the device's rates scale with its demand.
//! Its engagement is the stride above one page and a seal step past its
//! slice, each read on the table, else `VACUOUS`.

use crate::driver::Violations;
use core::cell::{Cell, RefCell};
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, Waker};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::rc::Rc;

use inf_foundation::hash64;
use inf_foundation::rng::{Entropy, SplitMix64};
use inf_log::fs::sim::SimDisk;
use inf_log::{
    TIER_FRAME_BYTES, TierFlush, TierFlushConfig, TierIoMode, tier_extract, tier_frame_offset,
    tier_frame_span,
};
use inf_runtime::{CellExecutor, WatermarkGate, WatermarkWait};
use inf_store::{
    AddressSpaceConfig, DemotionConfig, KeyHasher, Keyspace, LogicalAddr, NsId, StoreConfig,
    TieredLookup, TieredTable,
};

const NS: NsId = NsId(88);
const PAGE: u64 = 4 << 10;
/// Virtual round length (one reactor MAINTAIN cadence).
const ROUND_NS: u64 = 1_000_000; // 1 ms
/// Scenario stall timeout (a tight knob so the wedge phase stays cheap;
/// the ADR-0053 default of 1000 ms is config, not this harness).
const STALL_TIMEOUT_NS: u64 = 250 * ROUND_NS;
const WRITERS: usize = 8;
const OPS_PER_ROUND: u64 = 4;
/// Wedge phase schedule, in rounds.
/// Earliest round the wedge may begin. The wedge itself triggers on the
/// first stall at or after this round (deterministic per seed): a stall
/// proves the RAM window is full, so wedging *then* guarantees parked
/// writers with no possible flush progress — the typed-timeout leg every
/// seed must exercise. (A fixed wedge round under the S11 pipeline let
/// fast seeds finish the storm before the wedge and test dead air.)
const WEDGE_EARLIEST_ROUND: u64 = 100;
const MAX_ROUNDS: u64 = 20_000;

/// The window a run presses (the module's two legs).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PressureLeg {
    /// A 256 KiB budget at 4 KiB pages: a 128-page ring, the seal marks at
    /// one page.
    Budget,
    /// A 16 MiB budget at 4 KiB pages: an 8,192-page ring, the seal marks at
    /// a stride of two pages.
    WideRing,
}

impl PressureLeg {
    pub const ALL: [PressureLeg; 2] = [PressureLeg::Budget, PressureLeg::WideRing];

    /// The leg's name on the command line.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            PressureLeg::Budget => "budget",
            PressureLeg::WideRing => "wide-ring",
        }
    }

    /// The leg named `name`, if any.
    #[must_use]
    pub fn parse(name: &str) -> Option<PressureLeg> {
        PressureLeg::ALL.into_iter().find(|leg| leg.name() == name)
    }

    fn shape(self) -> Shape {
        match self {
            PressureLeg::Budget => Shape {
                budget: 256 << 10,
                value_len_min: 96,
                value_len_span: 128,
                ops_per_writer: 400,
                device_full_bytes_per_round: 64 << 10,
                device_throttled_bytes_per_round: 480,
            },
            // The storm's demand is about 8 × 4 × 2 KiB = 64 KiB a round:
            // full speed four times that, the throttled phase about a tenth,
            // so the window (4,097 pages) fills in about 300 rounds, past the
            // wedge's earliest round, and 1,600 ops a writer keep writing
            // through the wedge and the drain.
            PressureLeg::WideRing => Shape {
                budget: 16 << 20,
                value_len_min: 1024,
                value_len_span: 2048,
                ops_per_writer: 1_600,
                device_full_bytes_per_round: 256 << 10,
                device_throttled_bytes_per_round: 6 << 10,
            },
        }
    }
}

/// A leg's sizes: the window the storm presses and the storm and device
/// that press it.
#[derive(Clone, Copy, Debug)]
struct Shape {
    /// The table's memory budget; its window is the budget and one slice.
    budget: u64,
    value_len_min: usize,
    /// Each value is `value_len_min` bytes and less than this many more.
    value_len_span: u64,
    ops_per_writer: u64,
    /// Device bandwidth: full speed exceeds the storm's demand; the
    /// throttled phase serves roughly a tenth of it.
    device_full_bytes_per_round: u64,
    device_throttled_bytes_per_round: u64,
}

/// Scenario knobs (the DSL v0 shape).
#[derive(Debug)]
pub struct PressureScenario {
    pub seed: u64,
    pub leg: PressureLeg,
}

impl PressureScenario {
    #[must_use]
    pub fn m4_pressure(seed: u64) -> PressureScenario {
        PressureScenario { seed, leg: PressureLeg::Budget }
    }
}

#[derive(Debug, Default)]
pub struct PressureReport {
    pub violations: Violations,
    pub stalls: u64,
    pub stall_timeouts: u64,
    /// Virtual stall latencies (park → wake), nanoseconds.
    pub stall_p50_ns: u64,
    pub stall_p99_ns: u64,
    pub peak_committed_bytes: u64,
    /// The table's seal-mark stride, read at the wide ring's audit (0 on the
    /// budget leg, which reads none).
    pub seal_mark_stride_bytes: u64,
    pub trace_hash: u64,
    pub state_hash: u64,
    state: crate::state::StateHash,
}

impl PressureReport {
    #[must_use]
    pub fn ok(&self) -> bool {
        self.violations.is_empty()
    }
}

/// Typed writer-side outcome of one stalled write (the wire `STALLED`
/// class in miniature — ADR-0053 D4).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum StallOutcome {
    Progress,
    TimedOut,
}

/// Waits for flushed-watermark progress or the virtual deadline, as the
/// production write path does: it compares the loop's clock with the
/// deadline each time a flush round wakes its stalled writers. Round wakes
/// re-run the deadline check; the gate wake delivers progress.
struct StallWait {
    gate: WatermarkWait,
    now: Rc<Cell<u64>>,
    deadline_ns: u64,
    round_wakers: Rc<RefCell<Vec<Waker>>>,
}

impl Future for StallWait {
    type Output = StallOutcome;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<StallOutcome> {
        let this = Pin::into_inner(self);
        if Pin::new(&mut this.gate).poll(cx).is_ready() {
            return Poll::Ready(StallOutcome::Progress);
        }
        if this.now.get() >= this.deadline_ns {
            return Poll::Ready(StallOutcome::TimedOut);
        }
        this.round_wakers.borrow_mut().push(cx.waker().clone());
        Poll::Pending
    }
}

/// One round-yield: parks until the harness's next round wake, so a
/// writer's op pacing rides the round clock, not the poll loop.
struct RoundYield {
    round_wakers: Rc<RefCell<Vec<Waker>>>,
    parked: bool,
}

impl Future for RoundYield {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = Pin::into_inner(self);
        if this.parked {
            return Poll::Ready(());
        }
        this.parked = true;
        this.round_wakers.borrow_mut().push(cx.waker().clone());
        Poll::Pending
    }
}

struct World {
    ks: Keyspace,
    leg: PressureLeg,
    shape: Shape,
    /// Data bytes the device may still absorb this round.
    device_credit: u64,
    gate: WatermarkGate,
    now: Rc<Cell<u64>>,
    round_wakers: Rc<RefCell<Vec<Waker>>>,
    /// Model: key → expected value (BTreeMap — deterministic audit
    /// order, L7).
    model: BTreeMap<Vec<u8>, Vec<u8>>,
    stalls: u64,
    timeouts: u64,
    stall_latencies_ns: Vec<u64>,
    oom_verdicts: u64,
    writers_done: usize,
    /// The wedge trigger (deterministic): set at the first stall at or
    /// after [`WEDGE_EARLIEST_ROUND`].
    wedge_from: Option<u64>,
    /// Writer-side violations (spin-guard trips), merged into the report.
    violations: Violations,
    trace_hash: u64,
}

impl World {
    fn table(&mut self) -> &mut TieredTable {
        self.ks.tiered_store_mut(NS).expect("materialized")
    }
}

/// One writer's storm: the leg's seeded upserts over a 128-key working set,
/// at most `OPS_PER_ROUND` per round, stalling typed on the budget window.
async fn writer_storm(world: Rc<RefCell<World>>, writer: usize, seed: u64) {
    let mut rng = SplitMix64::new(seed ^ ((writer as u64) << 32));
    let mut in_round = 0u64;
    let shape = world.borrow().shape;
    for op in 0..shape.ops_per_writer {
        if in_round >= OPS_PER_ROUND {
            let wakers = Rc::clone(&world.borrow().round_wakers);
            RoundYield { round_wakers: wakers, parked: false }.await;
            in_round = 0;
        }
        in_round += 1;
        let key = format!("w{writer}:{:04}", rng.next_u64() % 128).into_bytes();
        let fill = (rng.next_u64() % 251) as u8;
        let len = shape.value_len_min + (rng.next_u64() % shape.value_len_span) as usize;
        let value = vec![fill; len];
        write_until_done(&world, &key, &value, op).await;
    }
    world.borrow_mut().writers_done += 1;
}

/// Consecutive same-instant Progress-then-refused retries tolerated
/// before the scenario declares a wake/retry arithmetic bug. With the
/// head-keyed gate a pre-woken waiter implies the retry fits, so this
/// never trips — it exists so a future regression surfaces as a
/// violation instead of an unbounded in-poll spin (the OOM class this
/// scenario once had: a flushed-keyed gate pre-woke waiters whose
/// release hadn't caught up yet).
const SPIN_GUARD: u32 = 1_000;

/// One op's write loop: attempt → stall typed → retry, until it lands or
/// times out. Only plain data and the waiter cross each suspension.
async fn write_until_done(world: &Rc<RefCell<World>>, key: &[u8], value: &[u8], op: u64) {
    let mut same_instant_retries = 0u32;
    loop {
        let (wait, started_ns) = {
            let world = &mut *world.borrow_mut();
            match try_upsert(world, key, value) {
                Ok(()) => return,
                Err(()) => {
                    let Some(target) = world.table().write_stall_target(key, value) else {
                        world.oom_verdicts += 1; // hard out-of-space — audited as a violation.
                        return;
                    };
                    world.stalls += 1;
                    let started_ns = world.now.get();
                    let round_now = started_ns / ROUND_NS;
                    if world.wedge_from.is_none() && round_now >= WEDGE_EARLIEST_ROUND {
                        // Arm the wedge two rounds out: this stall (and
                        // every one after it) parks into a dead device.
                        world.wedge_from = Some(round_now + 2);
                    }
                    let wait = StallWait {
                        gate: world.gate.waiter(target.to_raw()),
                        now: Rc::clone(&world.now),
                        deadline_ns: started_ns + STALL_TIMEOUT_NS,
                        round_wakers: Rc::clone(&world.round_wakers),
                    };
                    (wait, started_ns)
                }
            }
        };
        let outcome = wait.await;
        let world = &mut *world.borrow_mut();
        world.stall_latencies_ns.push(world.now.get() - started_ns);
        match outcome {
            StallOutcome::Progress => {
                // Woken: with the gate keyed on the post-release head,
                // the retry fits by arithmetic. The guard bounds the
                // loop anyway (put a limit on everything).
                if world.now.get() == started_ns {
                    same_instant_retries += 1;
                    if same_instant_retries > SPIN_GUARD {
                        world.violations.push(format!(
                            "stall wake/retry spin for {} (gate key outran the release slice)",
                            String::from_utf8_lossy(key)
                        ));
                        return;
                    }
                } else {
                    same_instant_retries = 0;
                }
            }
            StallOutcome::TimedOut => {
                // The typed `STALLED` verdict: surface it, never spin.
                world.timeouts += 1;
                world.trace_hash = hash64(key, world.trace_hash ^ 0x57A1_1ED0 ^ op);
                return;
            }
        }
    }
}

/// The mutation attempt: upsert through the routed entry; `Err(())` is
/// the budget-window refusal (the stall signal).
fn try_upsert(world: &mut World, key: &[u8], value: &[u8]) -> Result<(), ()> {
    let hash = world.table().hash_key(key);
    let found = match world.table().lookup(key, hash, &[]) {
        TieredLookup::Ram(addr) => {
            let parts = world.table().record(addr);
            Some((addr, parts.encoded_len, parts.version))
        }
        // Cold: the model supplies len (index-only on the write path,
        // §3.3). An unmodeled cold candidate is a 2⁻²² fingerprint
        // collision for an absent key — treated as a miss.
        TieredLookup::Cold(addr) => world
            .model
            .get(key)
            .map(|old| (addr, TieredTable::RECORD_HEADER_LEN + key.len() + old.len(), 0)),
        TieredLookup::Miss => None,
    };
    let result = match found {
        Some((addr, len, version)) => {
            world.table().update(key, value, hash, addr, len, version).map(|_| ())
        }
        None => world.table().insert(key, value, hash).map(|_| ()),
    };
    result.map_err(|_| ())?;
    let placed = match world.table().lookup(key, hash, &[]) {
        TieredLookup::Ram(addr) => addr,
        other @ (TieredLookup::Cold(_) | TieredLookup::Miss) => {
            panic!("fresh write must be RAM-resident: {other:?}")
        }
    };
    world.model.insert(key.to_vec(), value.to_vec());
    world.trace_hash = hash64(value, world.trace_hash ^ placed.to_raw());
    Ok(())
}

/// The S11 flush leg proper — [`TierFlush`] + `TieredTable::flush_slice`
/// under this round's device credit (the M4-S07 harness leg retired):
/// budgeted slices, capacity/gap rotation with footers (ADR-0056 D1/D2),
/// the partial-frame claim rule (D5), and the barrier seal that keeps
/// stalled writers wakeable when the pipeline runs dry (D8). The fleet
/// is the multi-file shape S12's MANIFEST later formalizes.
struct TierFleet {
    disk: SimDisk,
    flush: TierFlush<SimDisk>,
}

impl TierFleet {
    fn new() -> TierFleet {
        let disk = SimDisk::new();
        let flush = TierFlush::new(
            disk.clone(),
            TierFlushConfig {
                shard_dir: PathBuf::from("node/shard-0"),
                cell: 0,
                ns: NS,
                mode: TierIoMode::Buffered,
                // Scenario-sized capacity: small enough that the storm
                // exercises capacity rotation (D2), not only gap seals.
                file_capacity: 8 * PAGE,
                slice_bytes: PAGE,
                record_len_max: 16 << 20,
            },
            0,
            &inf_foundation::bounded::CapCensus::new(inf_foundation::CellId(0)).0,
        );
        TierFleet { disk, flush }
    }

    fn flush_round(&mut self, world: &mut World) {
        // The device's byte pacing vs the pipeline's slice granularity:
        // credit banks across rounds and a whole slice spends only when
        // the bank covers it, so the throttled phase serves its ~480
        // B/round *average* while slices stay the pipeline's real
        // quantum (min-progress chunks would otherwise turn every round
        // into a full slice and dissolve the backpressure this scenario
        // exists to exercise).
        let mut appended_total = 0u64;
        while world.device_credit >= PAGE {
            let outcome = world.table().flush_slice(&mut self.flush).expect("sim flush slice");
            if outcome.appended_bytes == 0 && outcome.gaps_crossed == 0 {
                break;
            }
            appended_total += outcome.appended_bytes;
            world.device_credit = world.device_credit.saturating_sub(outcome.appended_bytes.max(1));
        }
        // Dry with holdback outstanding while the device has credit: the
        // barrier seal (ADR-0056 D8) — without it a stalled writer waits
        // forever on bytes only its own (stalled) appends could finalize.
        if world.device_credit >= PAGE
            && appended_total == 0
            && self.flush.append_cursor().is_some()
        {
            world.table().flush_barrier(&mut self.flush).expect("sim barrier seal");
        }
    }

    /// Reads one record back from the simulated device's actual bytes
    /// (CRC-verified) — the final audit's cold path.
    fn read_record(&self, addr: u64, len: usize) -> Option<Vec<u8>> {
        let contains = |base: u64, flen: u64| addr >= base && addr + len as u64 <= base + flen;
        let (base, path) = self
            .flush
            .sealed()
            .iter()
            .find(|m| contains(m.base.to_raw(), m.data_len))
            .map(|m| (m.base.to_raw(), m.path.clone()))
            .or_else(|| {
                let (_, base, _, durable_len, path) = self.flush.active()?;
                contains(base.to_raw(), durable_len).then(|| (base.to_raw(), path.to_path_buf()))
            })?;
        let image = self.disk.contents(&path)?;
        let (first, count, skip) = tier_frame_span(addr - base, len);
        let from = tier_frame_offset(first) as usize;
        let to = from + count as usize * TIER_FRAME_BYTES;
        let mut out = Vec::new();
        tier_extract(image.get(from..to)?, skip, len, &mut out).ok()?;
        Some(out)
    }
}

fn build_world(leg: PressureLeg, seed: u64) -> Rc<RefCell<World>> {
    let shape = leg.shape();
    let demote = DemotionConfig::for_budget(shape.budget, PAGE);
    let census = inf_foundation::bounded::CapCensus::new(inf_foundation::CellId(0)).0;
    let ring = demote.ring_reserve_bytes().expect("valid budget");
    let mut ks =
        Keyspace::new(StoreConfig { hasher: KeyHasher::from_seed(seed), ..Default::default() });
    assert!(
        ks.materialize_tiered(
            NS,
            AddressSpaceConfig {
                reserve_bytes: ring,
                page_bytes: PAGE as usize,
                life_origin: LogicalAddr::ZERO,
            },
            demote,
            1024,
            &census,
        )
        .is_ok()
    );
    Rc::new(RefCell::new(World {
        ks,
        leg,
        shape,
        device_credit: 0,
        gate: WatermarkGate::new(),
        now: Rc::new(Cell::new(0)),
        round_wakers: Rc::new(RefCell::new(Vec::new())),
        model: BTreeMap::new(),
        stalls: 0,
        timeouts: 0,
        stall_latencies_ns: Vec::new(),
        oom_verdicts: 0,
        writers_done: 0,
        wedge_from: None,
        violations: Violations::new(),
        trace_hash: 0,
    }))
}

/// This round's device credit under the three-phase schedule.
fn device_credit_for(shape: &Shape, round: u64, wedge_from: Option<u64>) -> u64 {
    let Some(from) = wedge_from else {
        return shape.device_throttled_bytes_per_round;
    };
    let wedge_end = from + STALL_TIMEOUT_NS / ROUND_NS + 50;
    if round < from {
        shape.device_throttled_bytes_per_round
    } else if round < wedge_end {
        0 // wedged
    } else {
        shape.device_full_bytes_per_round
    }
}

/// One reactor round: EXECUTE slice → MAINTAIN (seal → flush-confirm →
/// release → gate advance) → RSS oracle → round clock + wakes.
fn run_round(
    world: &Rc<RefCell<World>>,
    fleet: &mut TierFleet,
    ex: &mut CellExecutor,
    report: &mut PressureReport,
    round: u64,
) {
    report.state.number(b"round-time", world.borrow().now.get());
    report.state.number(b"round", round);
    {
        // Bank this round's credit (bounded: full-speed rounds cover
        // demand outright, so the bank never grows past a few slices).
        let world = &mut *world.borrow_mut();
        let shape = world.shape;
        let credit = device_credit_for(&shape, round, world.wedge_from);
        world.device_credit = (world.device_credit + credit).min(shape.device_full_bytes_per_round);
        if credit == 0 {
            world.device_credit = 0; // wedged: the device is gone, not saving up
        }
    }
    ex.run_ready(256);
    {
        let world = &mut *world.borrow_mut();
        world.ks.demote_tick();
        fleet.flush_round(world);
        world.ks.demote_tick();
        // The gate advances with the **post-release head** (ADR-0053
        // D4): the stall target is the head value that makes the refused
        // alloc fit, and release — not flush confirmation alone — is
        // what moves it. Keying on `flushed` pre-wakes waiters whose
        // release slice hasn't caught up yet, which turns the retry loop
        // into an unbounded same-instant spin (the OOM class the
        // SPIN_GUARD documents).
        let head = world.table().space().head().to_raw();
        world.gate.advance(head);
        let committed = world.table().space().report().committed_bytes;
        report.peak_committed_bytes = report.peak_committed_bytes.max(committed);
        let budget = world.shape.budget;
        if committed > budget + PAGE {
            report.violations.push(format!(
                "round {round}: committed {committed} exceeds budget {budget} + slice {PAGE}"
            ));
        }
    }
    let (now, wakers) = {
        let world = world.borrow();
        (Rc::clone(&world.now), Rc::clone(&world.round_wakers))
    };
    now.set(now.get() + ROUND_NS);
    for waker in wakers.borrow_mut().drain(..) {
        waker.wake();
    }
    ex.run_ready(256);
}

/// Post-storm: drain demotion debt at full speed, seal the fleet.
fn drain(world: &Rc<RefCell<World>>, fleet: &mut TierFleet) {
    let world = &mut *world.borrow_mut();
    loop {
        world.device_credit = world.shape.device_full_bytes_per_round;
        let first = world.ks.demote_tick();
        fleet.flush_round(world);
        let second = world.ks.demote_tick();
        let progress =
            first.sealed_bytes + first.released_bytes + second.sealed_bytes + second.released_bytes;
        if progress == 0 {
            break;
        }
    }
    // Final drain: everything sealed flushes, the active file seals, and
    // `flushed` confirms to the sealed end (partial tail frame included —
    // the seal ends all rewrites, ADR-0056 D5).
    world.table().flush_drain(&mut fleet.flush).expect("final flush drain");
    while world.table().release_slice() > 0 {}
    let head = world.table().space().head().to_raw();
    world.gate.advance(head);
}

/// Liveness/typed-outcome oracles + the byte-exact content audit.
fn audit(world: &Rc<RefCell<World>>, fleet: &TierFleet, report: &mut PressureReport) {
    let world = &mut *world.borrow_mut();
    report.violations.append(&mut world.violations);
    if world.stalls == 0 {
        report.violations.push("throttled phase produced no stalls".into());
    }
    if world.timeouts == 0 {
        report.violations.push("wedged phase produced no typed timeouts".into());
    }
    if world.oom_verdicts > 0 {
        report
            .violations
            .push(format!("{} out-of-memory verdicts (must be 0)", world.oom_verdicts));
    }
    let counters = world.ks.tiering_counters();
    if counters.tail_alloc_stalls != world.stalls {
        report.violations.push(format!(
            "stall tripwire {} disagrees with writer stalls {}",
            counters.tail_alloc_stalls, world.stalls
        ));
    }
    match world.leg {
        PressureLeg::Budget => {}
        PressureLeg::WideRing => {
            stride_engagement(world, counters.demote_slices, counters.demote_sealed_bytes, report);
        }
    }
    let keys: Vec<(Vec<u8>, Vec<u8>)> =
        world.model.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    for (key, want) in keys {
        match read_back(world, fleet, &key, want.len()) {
            Some(value) if value == want => {
                report.trace_hash = hash64(&value, report.trace_hash);
            }
            Some(_) => report
                .violations
                .push(format!("value mismatch for {}", String::from_utf8_lossy(&key))),
            None => {
                report.violations.push(format!("record lost: {}", String::from_utf8_lossy(&key)));
            }
        }
    }
    report.stalls = world.stalls;
    report.stall_timeouts = world.timeouts;
    let mut latencies = world.stall_latencies_ns.clone();
    latencies.sort_unstable();
    let pick = |p: f64| match latencies.len() {
        0 => 0,
        n => latencies[((n as f64 * p) as usize).min(n - 1)],
    };
    report.stall_p50_ns = pick(0.50);
    report.stall_p99_ns = pick(0.99);
    report.trace_hash = hash64(
        &report.stalls.to_le_bytes(),
        report.trace_hash ^ world.trace_hash ^ report.stall_timeouts,
    );
}

/// The wide ring's engagement: the table's seal marks bucket at more than one
/// commit page, and some seal step sealed past its one-page slice, which only
/// a stride can make it do: the mean of the steps' sealed bytes is above the
/// slice. Either missing is `VACUOUS`, never a green run.
fn stride_engagement(
    world: &mut World,
    slices: u64,
    sealed_bytes: u64,
    report: &mut PressureReport,
) {
    let stride = world.table().seal_mark_stride_bytes();
    report.seal_mark_stride_bytes = stride;
    if stride <= PAGE {
        report.violations.push(format!(
            "VACUOUS pressure-stride-unengaged: the seal marks' stride is {stride} B, one commit \
             page of {PAGE} B, on a ring of {} pages",
            world.table().space().ring_bytes() / PAGE
        ));
    }
    if sealed_bytes <= slices.saturating_mul(PAGE) {
        report.violations.push(format!(
            "VACUOUS pressure-overrun-unengaged: {slices} seal steps sealed {sealed_bytes} B, \
             none past its {PAGE} B slice on the mean"
        ));
    }
}

/// Ground-truth read: RAM via the table, cold via the device's bytes,
/// fingerprint false positives excluded and retried.
fn read_back(world: &mut World, fleet: &TierFleet, key: &[u8], want_len: usize) -> Option<Vec<u8>> {
    let hash = world.table().hash_key(key);
    let mut exclude: Vec<LogicalAddr> = Vec::new();
    loop {
        match world.table().lookup(key, hash, &exclude) {
            TieredLookup::Ram(addr) => return Some(world.table().record(addr).value.to_vec()),
            TieredLookup::Cold(addr) => {
                let len = TieredTable::RECORD_HEADER_LEN + key.len() + want_len;
                let bytes = fleet.read_record(addr.to_raw(), len)?;
                let parts = TieredTable::decode_record(&bytes);
                if parts.key == key {
                    return Some(parts.value.to_vec());
                }
                exclude.push(addr); // fingerprint false positive
            }
            TieredLookup::Miss => return None,
        }
    }
}

/// Runs the scenario once. Deterministic from `scenario.seed` (L7).
#[must_use]
pub fn run_pressure_scenario(scenario: &PressureScenario) -> PressureReport {
    let mut report = PressureReport::default();
    let world = build_world(scenario.leg, scenario.seed);
    let mut fleet = TierFleet::new();
    let mut ex = CellExecutor::new(64);
    for writer in 0..WRITERS {
        let world = Rc::clone(&world);
        let _ = ex.poll_immediate(writer_storm(world, writer, scenario.seed ^ 0x5701_2233));
    }
    let mut round = 0u64;
    loop {
        round += 1;
        if round > MAX_ROUNDS {
            report.violations.push("storm did not drain (liveness violation)".into());
            break;
        }
        run_round(&world, &mut fleet, &mut ex, &mut report, round);
        if world.borrow().writers_done == WRITERS {
            break;
        }
    }
    drain(&world, &mut fleet);
    audit(&world, &fleet, &mut report);
    let world = world.borrow();
    report.state.number(b"finish-time", world.now.get());
    report.state.keyspace(&world.ks, inf_foundation::time::Nanos(world.now.get()));
    report.state.disk(&fleet.disk);
    report.state_hash = report.state.value();
    report
}
