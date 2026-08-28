//! `m4-cold` (M4-S08): the hardened cold-read DST — randomized region
//! placement, cold reads racing copy-to-tail and relocation, chunked
//! staging for oversized records, cancellation mid-flight, and the
//! compaction-under-flood unlink discipline (§3.3 pins) — all through
//! the **real** custody machinery (`ColdReads` + `IoGate` + executor
//! suspension) over the simulated disk.
//!
//! Per round: new GET futures spawn (some targeting cold records, some
//! RAM, some absent keys; a seeded fraction abandons its read mid-flight
//! — the client-disconnect leg the M0 custody rule exists for), the
//! driver pumps, mutations churn the same keys (relocations, deletes,
//! inserts — **never a cold read on the write/delete path**, asserted
//! per round via the issue counter), a relocation wave empties the
//! oldest file and unlinks it **only after its in-flight pins drain**
//! (deferrals must be observed under flood), and the demotion +
//! per-round flush leg keeps minting new cold files.
//!
//! The oracle is single-key linearizability over a per-key op-sequence
//! history (content + version, never addresses — §3.1): a GET's answer
//! must match some state the key held between the GET's first and last
//! observation points. Custody reconciles to zero at the end. Every
//! event folds into `trace_hash`; `--verify-determinism` requires
//! two-run identity (L7).

use core::cell::RefCell;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::rc::Rc;

use inf_alloc::BufferPool;
use inf_foundation::hash64;
use inf_foundation::rng::{Entropy, SplitMix64};
use inf_log::fs::SegmentFs;
use inf_log::fs::sim::SimDisk;
use inf_log::{
    TIER_FRAME_BYTES, TIER_FRAME_DATA, TierIoMode, TierWriter, tier_extract, tier_frame_offset,
    tier_frame_span,
};
use inf_runtime::{BackendDriver, CellExecutor, ColdReads, RawFd, TierFileId, TokenClass, Wait};
use inf_store::{
    AddressSpaceConfig, DemotionConfig, KeyHasher, Keyspace, LogicalAddr, NsId, StoreConfig,
    TieredLookup, TieredTable,
};

use crate::net::{CellNet, Plant, SimDriver};

const NS: NsId = NsId(99);
const PAGE: u64 = 4 << 10;
const BUDGET: u64 = 512 << 10;
/// Pool: 4 buffers × 4 frames — oversized records (~20–40 KiB) stage
/// through 2–3 chunks, and the flood can drain the pool (PoolDry is
/// backpressure, retried next round).
const POOL_BUFFERS: usize = 4;
const POOL_FRAMES: usize = 4;
/// S10 shaping under test: the cap sits BELOW the pool (3 < 4) so the
/// policy limit binds before the sizing limit — the storm proves the cap
/// holds and the overflow FIFO drains, and merging is on (ADR-0055).
const QD_CAP: usize = 3;
/// Virtual time per round (µs) — the injected-clock source for the
/// latency histogram (L7: no ambient clocks).
const ROUND_US: u64 = 1000;
const KEYS: u64 = 512;
const GETS_PER_ROUND: usize = 4;
const MUTATIONS_PER_ROUND: usize = 8;
/// Relocation ("compaction") wave cadence, rounds.
const WAVE_EVERY: u64 = 32;
/// Abandon (client-disconnect) probability, per GET, in permille.
const CANCEL_PERMILLE: u64 = 30;

/// Scenario knobs. `ops` counts every GET + mutation (the AC's 10⁶-op
/// run sets it explicitly; sim-smoke runs a lighter default).
#[derive(Debug)]
pub struct ColdStormScenario {
    pub seed: u64,
    pub ops: u64,
}

impl ColdStormScenario {
    #[must_use]
    pub fn m4_cold(seed: u64) -> ColdStormScenario {
        ColdStormScenario { seed, ops: 20_000 }
    }
}

#[derive(Debug, Default)]
pub struct ColdStormReport {
    pub violations: Vec<String>,
    pub gets: u64,
    pub cold_served: u64,
    pub chunked_reads: u64,
    pub restarts: u64,
    pub cancelled: u64,
    /// Cancelled while still queued (the drain skipped the intent).
    pub cancelled_early: u64,
    /// Cancelled after delivery (the parked `ColdDone` dropped).
    pub cancelled_late: u64,
    /// Waiters that rode a merged device read (coalescing observed).
    pub merged_waiters: u64,
    pub queue_high_water: u32,
    pub unlink_deferrals: u64,
    pub unlinks: u64,
    pub trace_hash: u64,
}

impl ColdStormReport {
    #[must_use]
    pub fn ok(&self) -> bool {
        self.violations.is_empty()
    }
}

/// One key's model: monotone per-key op sequence → observable state.
/// `None` = absent (deleted). History is full-fidelity — the oracle
/// checks answers against every state the key held in the GET's window.
#[derive(Default)]
struct KeyHistory {
    seq: u64,
    states: Vec<(u64, Option<(u32, u64)>)>, // (seq, Some((version, value_hash))) | None
}

impl KeyHistory {
    fn push(&mut self, state: Option<(u32, u64)>) {
        self.seq += 1;
        self.states.push((self.seq, state));
    }

    /// Is `answer` a state this key held in `[from_seq, to_seq]`?
    fn accepts(&self, from_seq: u64, to_seq: u64, answer: Option<(u32, u64)>) -> bool {
        // The state at `from_seq` is the last entry ≤ from_seq; every
        // entry inside the window is also legal (the read may linearize
        // anywhere within it).
        let mut current: Option<(u32, u64)> = None;
        let mut legal = false;
        for &(seq, state) in &self.states {
            if seq > to_seq {
                break;
            }
            current = state;
            if seq >= from_seq && state == answer {
                legal = true;
            }
        }
        legal || current == answer || (self.states.is_empty() && answer.is_none())
    }
}

/// One sealed tier file (a per-round flush unit) and its live set.
struct FleetFile {
    file: TierFileId,
    base: u64,
    len: u64,
    frames: u64,
    fd: RawFd,
    path: PathBuf,
    /// Keys whose current record lives in this file (repoints remove).
    live: Vec<Vec<u8>>,
    unlinked: bool,
}

struct World {
    ks: Keyspace,
    cold: ColdReads,
    driver: SimDriver,
    disk: SimDisk,
    shard_dir: PathBuf,
    files: Vec<FleetFile>,
    next_file_id: u32,
    /// Allocation log in address order (addr, len, key) — flush source +
    /// per-file live-set input.
    log: Vec<(u64, usize, Vec<u8>)>,
    flush_cursor: usize,
    model: BTreeMap<Vec<u8>, KeyHistory>,
    /// Current live value bytes per key (mutation input; the history
    /// stores hashes).
    values: BTreeMap<Vec<u8>, Vec<u8>>,
    /// Injected virtual clock (µs), advanced once per round (L7).
    now_us: u64,
    report: ColdStormReport,
}

impl World {
    fn table(&mut self) -> &mut TieredTable {
        self.ks.tiered_store_mut(NS).expect("materialized")
    }

    fn find_file(&self, addr: u64) -> Option<&FleetFile> {
        self.files.iter().find(|f| addr >= f.base && addr < f.base + f.len)
    }

    fn violation(&mut self, message: String) {
        self.report.violations.push(message);
    }
}

/// A GET's terminal answer, oracle-checked by the harness.
struct GetAnswer {
    key: Vec<u8>,
    answer: Option<(u32, u64)>, // (version, value_hash)
    from_seq: u64,
    to_seq: u64,
    served_cold: bool,
    chunks: u32,
    cancelled: bool,
}

/// What one lookup+issue planning borrow produced.
enum Plan {
    Ram(Option<(u32, u64)>),
    Fetch {
        addr: LogicalAddr,
        wait: inf_runtime::ColdWait,
        window_frames: u64,
        skip: usize,
    },
    /// Pool dry — backpressure; retry next poll round.
    Dry,
    Corrupt(String),
}

/// Plans one read attempt under a single short borrow: resolve, map the
/// address to its pinned file, lease + issue the aligned window.
fn plan_read(world: &Rc<RefCell<World>>, key: &[u8], hash: u64, exclude: &[LogicalAddr]) -> Plan {
    let world = &mut *world.borrow_mut();
    match world.table().lookup(key, hash, exclude) {
        TieredLookup::Ram(addr) => {
            let parts = world.table().record(addr);
            Plan::Ram(Some((parts.version, hash64(parts.value, 0))))
        }
        TieredLookup::Miss => Plan::Ram(None),
        TieredLookup::Cold(addr) => {
            let Some((fd, file, frames, base)) =
                world.find_file(addr.to_raw()).map(|f| (f.fd, f.file, f.frames, f.base))
            else {
                return Plan::Corrupt(format!("cold addr {} maps to no file", addr.to_raw()));
            };
            let (first, _, skip) =
                tier_frame_span(addr.to_raw() - base, TieredTable::RECORD_HEADER_LEN);
            let window_frames = (POOL_FRAMES as u64).min(frames - first);
            let len = (window_frames as usize) * TIER_FRAME_BYTES;
            let now_us = world.now_us;
            match world.cold.enqueue(
                fd,
                file,
                tier_frame_offset(first),
                len,
                inf_runtime::ReadClass::Foreground,
                now_us,
            ) {
                Ok(wait) => Plan::Fetch { addr, wait, window_frames, skip },
                Err(_) => Plan::Dry,
            }
        }
    }
}

/// Continuation-chunk issue (skip is zero by frame arithmetic — asserted
/// at the call site).
fn plan_chunk(world: &Rc<RefCell<World>>, addr: u64, done: usize, remaining: usize) -> Plan {
    let world = &mut *world.borrow_mut();
    let Some((fd, file, frames, base)) =
        world.find_file(addr).map(|f| (f.fd, f.file, f.frames, f.base))
    else {
        return Plan::Corrupt(format!("staging addr {addr} maps to no file"));
    };
    let (first, _, skip) = tier_frame_span(addr - base + done as u64, remaining);
    assert_eq!(skip, 0, "continuation chunks start frame-aligned");
    let window_frames = (POOL_FRAMES as u64).min(frames - first);
    let len = (window_frames as usize) * TIER_FRAME_BYTES;
    let now_us = world.now_us;
    match world.cold.enqueue(
        fd,
        file,
        tier_frame_offset(first),
        len,
        inf_runtime::ReadClass::Foreground,
        now_us,
    ) {
        Ok(wait) => Plan::Fetch {
            addr: LogicalAddr::from_raw(addr).expect("fits"),
            wait,
            window_frames,
            skip,
        },
        Err(_) => Plan::Dry,
    }
}

/// The hardened GET: fetch-verify-retry with re-resolution after every
/// resume, bounded chunked staging for oversized records, and bounded
/// restarts when relocation moves the record mid-read.
async fn cold_get(world: Rc<RefCell<World>>, key: Vec<u8>, cancel_roll: u64) -> GetAnswer {
    let hash = world.borrow().ks.hasher().hash(&key);
    let from_seq = world.borrow().model.get(&key).map_or(0, |h| h.seq);
    let mut answer = GetAnswer {
        key: key.clone(),
        answer: None,
        from_seq,
        to_seq: 0,
        served_cold: false,
        chunks: 0,
        cancelled: false,
    };
    let mut exclude: Vec<LogicalAddr> = Vec::new();
    let mut restarts = 0u32;
    'attempt: loop {
        if restarts > 32 {
            world.borrow_mut().violation(format!(
                "GET livelock (>32 restarts) for {}",
                String::from_utf8_lossy(&key)
            ));
            break;
        }
        let plan = plan_read(&world, &key, hash, &exclude);
        let (addr, wait, window_frames, skip) = match plan {
            Plan::Ram(state) => {
                answer.answer = state;
                break;
            }
            Plan::Fetch { addr, wait, window_frames, skip } => (addr, wait, window_frames, skip),
            Plan::Dry => {
                // Pool backpressure (S10 shapes it properly): retry after
                // the round turns — never counted against the relocation
                // restart cap.
                YieldOnce::default().await;
                continue;
            }
            Plan::Corrupt(message) => {
                world.borrow_mut().violation(message);
                break;
            }
        };
        // The client-disconnect legs (both cancellation interleavings the
        // S10 queue model exposes; custody is the end-of-run reconcile's
        // job either way):
        //   early — dropped while still queued: the drain must skip it
        //   and spend no device read (`cancelled_queued` counts it);
        //   late  — dropped after the round's drain+completion delivered
        //   the value: the parked `ColdDone` releases on drop.
        if cancel_roll < CANCEL_PERMILLE {
            let early = cancel_roll < CANCEL_PERMILLE / 2;
            if !early {
                YieldOnce::default().await; // let the round drain + deliver
            }
            drop(wait);
            answer.cancelled = true;
            let world = &mut *world.borrow_mut();
            world.report.cancelled += 1;
            if early {
                world.report.cancelled_early += 1;
            } else {
                world.report.cancelled_late += 1;
            }
            break;
        }
        let done = wait.await; // ← suspension: plain data only
        if let Err(errno) = done.outcome() {
            world
                .borrow_mut()
                .violation(format!("cold read errno {errno} (pin discipline broken?)"));
            break;
        }
        // Re-resolve before trusting anything (the M0 custody rule).
        enum After {
            Serve(Option<(u32, u64)>),
            Stage { total: usize, assembled: Vec<u8> },
            Retry,
            Restart,
        }
        let after = {
            let w = &mut *world.borrow_mut();
            match w.table().lookup(&key, hash, &exclude) {
                TieredLookup::Ram(promoted) => {
                    let parts = w.table().record(promoted);
                    After::Serve(Some((parts.version, hash64(parts.value, 0))))
                }
                TieredLookup::Miss => After::Serve(None),
                TieredLookup::Cold(now) if now == addr => done.bytes(|window| {
                    let mut head = Vec::new();
                    if tier_extract(window, skip, TieredTable::RECORD_HEADER_LEN, &mut head)
                        .is_err()
                    {
                        w.report.violations.push("tier frame CRC failed".into());
                        return After::Serve(None);
                    }
                    let total = TieredTable::record_len_from_header(&head);
                    let window_data = window_frames as usize * TIER_FRAME_DATA;
                    if skip + total <= window_data {
                        let mut record = Vec::new();
                        tier_extract(window, skip, total, &mut record).expect("verified above");
                        let parts = TieredTable::decode_record(&record);
                        if parts.key == key {
                            After::Serve(Some((parts.version, hash64(parts.value, 0))))
                        } else {
                            After::Retry // 2⁻²² fingerprint false positive
                        }
                    } else {
                        // Oversized: keep the first window's payload and
                        // stage the rest chunk by chunk.
                        let take = window_data - skip;
                        let mut assembled = Vec::with_capacity(total);
                        tier_extract(window, skip, take, &mut assembled).expect("verified above");
                        After::Stage { total, assembled }
                    }
                }),
                TieredLookup::Cold(_) => After::Restart,
            }
        };
        drop(done); // custody back before any further await
        match after {
            After::Serve(state) => {
                answer.answer = state;
                answer.served_cold = true;
                break;
            }
            After::Retry => {
                exclude.push(addr);
                continue;
            }
            After::Restart => {
                restarts += 1;
                continue;
            }
            After::Stage { total, mut assembled } => {
                world.borrow_mut().report.chunked_reads += 1;
                while assembled.len() < total {
                    answer.chunks += 1;
                    let remaining = total - assembled.len();
                    let plan = plan_chunk(&world, addr.to_raw(), assembled.len(), remaining);
                    let (wait, window_frames) = match plan {
                        Plan::Fetch { wait, window_frames, .. } => (wait, window_frames),
                        Plan::Dry => {
                            YieldOnce::default().await;
                            continue;
                        }
                        Plan::Corrupt(message) => {
                            world.borrow_mut().violation(message);
                            break 'attempt;
                        }
                        Plan::Ram(_) => unreachable!("plan_chunk never resolves"),
                    };
                    let done = wait.await;
                    if let Err(errno) = done.outcome() {
                        world.borrow_mut().violation(format!("staging read errno {errno}"));
                        break 'attempt;
                    }
                    let w = &mut *world.borrow_mut();
                    match w.table().lookup(&key, hash, &exclude) {
                        TieredLookup::Cold(now) if now == addr => {
                            let ok = done.bytes(|window| {
                                let take = remaining.min(window_frames as usize * TIER_FRAME_DATA);
                                let mut piece = Vec::new();
                                if tier_extract(window, 0, take, &mut piece).is_err() {
                                    return false;
                                }
                                assembled.extend_from_slice(&piece);
                                true
                            });
                            if !ok {
                                w.report.violations.push("staging frame CRC failed".into());
                                break 'attempt;
                            }
                        }
                        TieredLookup::Ram(promoted) => {
                            // Promoted mid-staging: abandon the stale
                            // prefix, serve RAM (fresher; both legal).
                            let parts = w.table().record(promoted);
                            answer.answer = Some((parts.version, hash64(parts.value, 0)));
                            drop(done);
                            break 'attempt;
                        }
                        TieredLookup::Miss => {
                            answer.answer = None;
                            drop(done);
                            break 'attempt;
                        }
                        TieredLookup::Cold(_) => {
                            drop(done);
                            restarts += 1;
                            continue 'attempt;
                        }
                    }
                }
                let parts = TieredTable::decode_record(&assembled);
                if parts.key == key {
                    answer.answer = Some((parts.version, hash64(parts.value, 0)));
                    answer.served_cold = true;
                    break;
                }
                exclude.push(addr);
            }
        }
    }
    {
        let world = &mut *world.borrow_mut();
        world.report.restarts += u64::from(restarts);
        answer.to_seq = world.model.get(&key).map_or(0, |h| h.seq);
    }
    answer
}

/// One cooperative yield (retry-next-round pacing for PoolDry).
#[derive(Default)]
struct YieldOnce {
    parked: bool,
}

impl core::future::Future for YieldOnce {
    type Output = ();

    fn poll(
        self: core::pin::Pin<&mut Self>,
        cx: &mut core::task::Context<'_>,
    ) -> core::task::Poll<()> {
        let this = core::pin::Pin::into_inner(self);
        if this.parked {
            return core::task::Poll::Ready(());
        }
        this.parked = true;
        cx.waker().wake_by_ref();
        core::task::Poll::Pending
    }
}

// ---- harness legs (synchronous, between executor slices) ------------------

/// Seeded mutations — relocations, deletes, inserts, exact-fit updates —
/// with the §3.3 assert: the write/delete path never issues a cold read.
fn mutate_round(world: &mut World, rng: &mut SplitMix64, ops: &mut u64, budget: u64) {
    let issued_before = world.cold.counters().issued;
    for _ in 0..MUTATIONS_PER_ROUND.min(budget as usize) {
        *ops += 1;
        let key = format!("k:{:04}", rng.next_u64() % KEYS).into_bytes();
        let roll = rng.next_u64() % 100;
        let hash = world.ks.hasher().hash(&key);
        let current = world.values.get(&key).cloned();
        match (roll, current) {
            // Delete: index + accounting only, even when cold.
            (0..=14, Some(value)) => {
                let found = lookup_addr(world, &key, hash);
                if let Some(addr) = found {
                    let len = TieredTable::RECORD_HEADER_LEN + key.len() + value.len();
                    world.table().delete(hash, addr, len);
                    note_repoint(world, &key, addr);
                    world.values.remove(&key);
                    world.model.entry(key).or_default().push(None);
                }
            }
            // Exact-fit update (in-place when mutable), else relocation.
            (15.., Some(value)) => {
                let same_len = roll < 25;
                let new_value = if same_len {
                    vec![(rng.next_u64() % 251) as u8; value.len()]
                } else {
                    seeded_value(rng)
                };
                upsert(world, &key, &new_value, hash);
            }
            (_, None) => {
                let value = seeded_value(rng);
                upsert(world, &key, &value, hash);
            }
        }
    }
    let issued_after = world.cold.counters().issued;
    if issued_after != issued_before {
        world.violation("mutation round issued a cold read (§3.3 index-only rule broken)".into());
    }
}

/// Mostly small values; occasionally oversized (chunked-staging food).
fn seeded_value(rng: &mut SplitMix64) -> Vec<u8> {
    let oversized = rng.next_u64().is_multiple_of(200);
    let len = if oversized {
        (20 << 10) + (rng.next_u64() % (20 << 10)) as usize
    } else {
        24 + (rng.next_u64() % 240) as usize
    };
    vec![(rng.next_u64() % 251) as u8; len]
}

fn lookup_addr(world: &mut World, key: &[u8], hash: u64) -> Option<LogicalAddr> {
    match world.table().lookup(key, hash, &[]) {
        TieredLookup::Ram(addr) | TieredLookup::Cold(addr) => Some(addr),
        TieredLookup::Miss => None,
    }
}

fn upsert(world: &mut World, key: &[u8], value: &[u8], hash: u64) {
    let old = world.values.get(key).map(Vec::len);
    let found = lookup_addr(world, key, hash);
    let placed = match (found, old) {
        (Some(addr), Some(old_len)) => {
            let len = TieredTable::RECORD_HEADER_LEN + key.len() + old_len;
            let version = world
                .model
                .get(key)
                .and_then(|h| h.states.last())
                .map_or(0, |s| s.1.map_or(0, |(v, _)| v));
            let placed = write_with_backpressure(world, |w| {
                w.table().update(key, value, hash, addr, len, version)
            });
            if placed != addr {
                note_repoint(world, key, addr);
            }
            placed
        }
        _ => write_with_backpressure(world, |w| w.table().insert(key, value, hash)),
    };
    let parts_version = world.table().record(placed).version;
    let len = world.table().record(placed).encoded_len;
    world.log.push((placed.to_raw(), len, key.to_vec()));
    world.values.insert(key.to_vec(), value.to_vec());
    world.model.entry(key.to_vec()).or_default().push(Some((parts_version, hash64(value, 0))));
}

/// The harness's synchronous backpressure: a budget-window refusal
/// drains one MAINTAIN round and retries (the m4-pressure scenario owns
/// the suspended-writer shape; here the storm must simply never wedge).
fn write_with_backpressure(
    world: &mut World,
    mut write: impl FnMut(&mut World) -> Result<LogicalAddr, inf_store::OpError>,
) -> LogicalAddr {
    for _ in 0..64 {
        match write(world) {
            Ok(placed) => return placed,
            Err(_) => maintain_round(world),
        }
    }
    panic!("mutation could not fit after 64 drain rounds (budget wedge)");
}

/// A repoint/delete left `addr`'s bytes dead: drop the key from its
/// containing file's live set (the S14 shape, harness-tracked).
fn note_repoint(world: &mut World, key: &[u8], addr: LogicalAddr) {
    let raw = addr.to_raw();
    if let Some(file) = world.files.iter_mut().find(|f| raw >= f.base && raw < f.base + f.len) {
        file.live.retain(|k| k != key);
    }
}

/// Demotion + the per-round flush leg: everything sealed this round
/// lands in one (or more, across ring holes) fresh tier files — sealed,
/// fdatasync'd, fd kept for cold reads, live set recorded.
fn maintain_round(world: &mut World) {
    world.ks.demote_tick();
    flush_all_sealed(world);
    world.ks.demote_tick();
}

fn flush_all_sealed(world: &mut World) {
    let ro = world.table().space().ro_boundary().to_raw();
    let flushed = world.table().space().flushed().to_raw();
    if flushed == ro {
        return;
    }
    let mut writer: Option<(TierWriter<SimDisk>, Vec<Vec<u8>>)> = None;
    let mut confirmed = flushed;
    while world.flush_cursor < world.log.len() {
        let (addr, len, key) = world.log[world.flush_cursor].clone();
        if addr + len as u64 > ro {
            break;
        }
        if addr >= flushed {
            let contiguous =
                writer.as_ref().is_some_and(|(w, _)| w.base().to_raw() + w.data_len() == addr);
            if !contiguous {
                seal_file(world, writer.take());
                let base = LogicalAddr::from_raw(addr).expect("fits");
                let disk = world.disk.clone();
                let w = TierWriter::create(
                    &disk,
                    &world.shard_dir,
                    world.next_file_id,
                    0,
                    NS,
                    base,
                    TierIoMode::Buffered,
                )
                .expect("tier file");
                world.next_file_id += 1;
                writer = Some((w, Vec::new()));
            }
            let bytes = world
                .table()
                .record_bytes(LogicalAddr::from_raw(addr).expect("fits"), len)
                .to_vec();
            let (w, live) = writer.as_mut().expect("ensured above");
            w.append(LogicalAddr::from_raw(addr).expect("fits"), &bytes).expect("append");
            // Only records still pointed at by the index are live here;
            // dead copies flush as raw bytes (the contiguous-range rule).
            let hash = world.ks.hasher().hash(&key);
            if lookup_addr(world, &key, hash).map(LogicalAddr::to_raw) == Some(addr) {
                live.push(key);
            }
            confirmed = addr + len as u64;
        }
        world.flush_cursor += 1;
    }
    seal_file(world, writer.take());
    if confirmed > flushed {
        world.table().space_mut().advance_flushed(LogicalAddr::from_raw(confirmed).expect("fits"));
    }
}

fn seal_file(world: &mut World, writer: Option<(TierWriter<SimDisk>, Vec<Vec<u8>>)>) {
    let Some((mut w, live)) = writer else { return };
    w.sync().expect("sim fdatasync before the watermark advances");
    let id = TierFileId::new(world.next_file_id - 1);
    world.files.push(FleetFile {
        file: id,
        base: w.base().to_raw(),
        len: w.data_len(),
        frames: w.data_len().div_ceil(TIER_FRAME_DATA as u64),
        fd: w.raw_fd().expect("sim files carry fake fds"),
        path: w.path().to_owned(),
        live,
        unlinked: false,
    });
}

/// The compaction-shaped wave: relocate every live record out of the
/// oldest populated file, then unlink it — **deferred while its
/// in-flight pin count is nonzero** (§3.3; the deferral must be
/// observed under flood for the AC).
fn relocation_wave(world: &mut World) {
    // Adversarial target choice (the DST's job): prefer a file the flood
    // is holding pins on right now — relocating its records under the
    // in-flight reads is the exact race the AC names, and its unlink
    // must then defer on the pins.
    let pinned = world
        .files
        .iter()
        .position(|f| !f.unlinked && !f.live.is_empty() && world.cold.inflight_on(f.file) > 0);
    let oldest = world.files.iter().position(|f| !f.unlinked && !f.live.is_empty());
    let Some(index) = pinned.or(oldest) else {
        unlink_drained(world);
        return;
    };
    let keys = world.files[index].live.clone();
    for key in keys {
        let hash = world.ks.hasher().hash(&key);
        let Some(value) = world.values.get(&key).cloned() else { continue };
        // Content-preserving relocation, modeled as an update (the real
        // S15 copy-forward preserves versions; this harness's oracle
        // tracks whatever version the mutation produces — content is
        // what must never be wrong).
        upsert(world, &key, &value, hash);
    }
    unlink_drained(world);
}

fn unlink_drained(world: &mut World) {
    let mut deferrals = 0u64;
    let mut unlinks = 0u64;
    for i in 0..world.files.len() {
        let (file, empty, path) = {
            let f = &world.files[i];
            (f.file, f.live.is_empty() && !f.unlinked, f.path.clone())
        };
        if !empty {
            continue;
        }
        if world.cold.inflight_on(file) > 0 {
            deferrals += 1; // pinned: the unlink waits for the drain.
            continue;
        }
        world.disk.remove_file(&path).expect("sim unlink");
        world.files[i].unlinked = true;
        unlinks += 1;
    }
    world.report.unlink_deferrals += deferrals;
    world.report.unlinks += unlinks;
    world.report.trace_hash = hash64(&unlinks.to_le_bytes(), world.report.trace_hash ^ deferrals);
}

/// Pump the sim driver and route TierRead completions into the custody
/// table (the plane's completion-dispatch role).
fn pump(world: &mut World, recv_pool: &mut BufferPool) {
    // S10: the drain admits queued intents (merge + QD cap + deficit)
    // into the same submit the eager path always rode.
    let cold = world.cold.clone();
    cold.drain(|op| world.driver.push(op));
    let mut out = Vec::new();
    world.driver.submit_and_reap(recv_pool, Wait::Poll, &mut out).expect("sim submit");
    for completion in out {
        assert_eq!(completion.token.class(), TokenClass::TierRead);
        world.cold.on_completion(completion.token, completion.result, world.now_us);
    }
}

/// Runs the scenario once. Deterministic from the seed (L7).
#[must_use]
pub fn run_cold_storm_scenario(scenario: &ColdStormScenario) -> ColdStormReport {
    let mut rng = SplitMix64::new(scenario.seed ^ 0xC01D_5701);
    let demote = DemotionConfig {
        mem_budget_bytes: BUDGET,
        mutable_permille: 60, // small mutable window ⇒ records age cold fast
        slice_bytes: 16 << 10,
    };
    let ring = demote.ring_reserve_bytes().expect("valid budget");
    let mut ks = Keyspace::new(StoreConfig {
        hasher: KeyHasher::from_seed(scenario.seed),
        ..Default::default()
    });
    assert!(
        ks.materialize_tiered(
            NS,
            AddressSpaceConfig {
                reserve_bytes: ring,
                page_bytes: PAGE as usize,
                life_origin: LogicalAddr::ZERO,
            },
            demote,
            KEYS as usize * 2,
        )
        .is_ok()
    );
    let disk = SimDisk::new();
    let world = Rc::new(RefCell::new(World {
        ks,
        cold: ColdReads::with_config(
            inf_alloc::AlignedPool::new(POOL_BUFFERS, POOL_FRAMES * TIER_FRAME_BYTES),
            inf_runtime::ColdReadConfig { qd_cap: QD_CAP, ..Default::default() },
        ),
        driver: SimDriver::with_disk(
            CellNet::new(0, scenario.seed ^ 0xD15C, Plant::None),
            disk.clone(),
        ),
        disk,
        shard_dir: PathBuf::from("node/shard-0"),
        files: Vec::new(),
        next_file_id: 0,
        log: Vec::new(),
        flush_cursor: 0,
        model: BTreeMap::new(),
        values: BTreeMap::new(),
        now_us: 0,
        report: ColdStormReport::default(),
    }));
    let mut ex = CellExecutor::new(256);
    let mut recv_pool = BufferPool::new(2, 4096);
    let answers: Rc<RefCell<Vec<GetAnswer>>> = Rc::new(RefCell::new(Vec::new()));

    let mut ops = 0u64;
    let mut round = 0u64;
    while ops < scenario.ops || ex.live_tasks() > 0 {
        round += 1;
        assert!(round < scenario.ops.max(1) * 4, "storm rounds exploded (liveness)");
        world.borrow_mut().now_us = round * ROUND_US;
        // Foreground: new GETs (cold-heavy once files exist).
        if ops < scenario.ops {
            for _ in 0..GETS_PER_ROUND {
                ops += 1;
                let absent = rng.next_u64().is_multiple_of(20);
                let key = if absent {
                    format!("absent:{:04}", rng.next_u64() % 64).into_bytes()
                } else {
                    format!("k:{:04}", rng.next_u64() % KEYS).into_bytes()
                };
                let cancel_roll = rng.next_u64() % 1000;
                let world_rc = Rc::clone(&world);
                let sink = Rc::clone(&answers);
                let _ = ex.poll_immediate(async move {
                    let answer = cold_get(world_rc, key, cancel_roll).await;
                    sink.borrow_mut().push(answer);
                });
            }
        }
        ex.run_ready(512); // issue: cold reads go in flight, pins held.
        // Mutations + waves + demotion run WHILE reads are in flight —
        // the racing window the AC names: relocation repoints addresses
        // under suspended reads, and the wave's unlink must defer on the
        // §3.3 pins the flood is holding.
        {
            let world = &mut *world.borrow_mut();
            if ops < scenario.ops {
                let budget = scenario.ops - ops;
                mutate_round(world, &mut rng, &mut ops, budget);
            }
            if round.is_multiple_of(WAVE_EVERY) {
                relocation_wave(world);
            }
            maintain_round(world);
            pump(world, &mut recv_pool);
        }
        ex.run_ready(512); // resume: re-resolve, decode, or restart.
        {
            let world = &mut *world.borrow_mut();
            pump(world, &mut recv_pool); // chunk continuations
        }
        ex.run_ready(512);
        // Oracle: verify every completed GET against the history model.
        {
            let world = &mut *world.borrow_mut();
            for answer in answers.borrow_mut().drain(..) {
                verify_answer(world, answer);
            }
        }
    }

    // Custody reconciles: no lease, pin, or in-flight entry survives.
    let world = &mut *world.borrow_mut();
    if let Err(leak) = world.cold.reconcile() {
        world.report.violations.push(format!("custody leak: {leak:?}"));
    }
    let counters = world.cold.counters();
    world.report.merged_waiters = counters.merged_waiters;
    world.report.queue_high_water = counters.queue_depth_high_water;
    // Both cancellation interleavings the queue model exposes must be
    // exercised (the sim device completes inline, so the mid-flight
    // unclaimed class belongs to the real-backend test, not this DST).
    if world.report.cancelled_early > 0 && counters.cancelled_queued == 0 {
        world.report.violations.push("early cancel never skipped at the drain".into());
    }
    if world.report.cancelled > 0
        && (world.report.cancelled_early == 0 || world.report.cancelled_late == 0)
    {
        world.report.violations.push("a cancellation leg went unexercised (knobs drifted)".into());
    }
    // The S10 admission identity at quiesce, and the cap that must hold.
    if counters.enqueued != counters.issued + counters.merged_waiters + counters.cancelled_queued {
        world.report.violations.push(format!(
            "admission identity broken: enqueued {} != issued {} + merged {} + cancelled_queued {}",
            counters.enqueued, counters.issued, counters.merged_waiters, counters.cancelled_queued
        ));
    }
    if world.cold.qd_percentile(100.0) > QD_CAP as u64 {
        world.report.violations.push(format!(
            "device QD exceeded the cap: sampled max {} > {QD_CAP}",
            world.cold.qd_percentile(100.0)
        ));
    }
    if scenario.ops >= 100_000 && counters.merged_waiters == 0 {
        world.report.violations.push("no coalescing observed at storm scale".into());
    }
    if world.report.chunked_reads == 0 {
        world.report.violations.push("no oversized record staged (knobs drifted)".into());
    }
    if world.report.unlink_deferrals == 0 {
        world.report.violations.push("no unlink deferral observed under flood".into());
    }
    let mut report = std::mem::take(&mut world.report);
    report.trace_hash = hash64(
        &counters.issued.to_le_bytes(),
        report.trace_hash ^ counters.completed ^ counters.unclaimed,
    );
    report
}

fn verify_answer(world: &mut World, answer: GetAnswer) {
    world.report.gets += 1;
    if answer.cancelled {
        return; // no answer to judge; custody is the reconcile's job.
    }
    world.report.cold_served += u64::from(answer.served_cold);
    let legal = match world.model.get(&answer.key) {
        Some(history) => history.accepts(answer.from_seq, answer.to_seq, answer.answer),
        None => answer.answer.is_none(),
    };
    if !legal {
        world.report.violations.push(format!(
            "wrong read for {}: {:?} not a state in [{}, {}]",
            String::from_utf8_lossy(&answer.key),
            answer.answer,
            answer.from_seq,
            answer.to_seq
        ));
    }
    world.report.trace_hash = hash64(
        &answer.key,
        world.report.trace_hash
            ^ answer.answer.map_or(0, |(v, h)| h ^ u64::from(v))
            ^ u64::from(answer.chunks),
    );
}
