//! `m4-steel` (M4-S04/S06): the steel-thread lifecycle under the
//! deterministic driver — the DST twin of
//! `inf-runtime/tests/steel_thread.rs`.
//!
//! One seeded life: write a corpus into a keyspace-materialized tiered
//! table → seal → flush to a tier file on the [`SimDisk`] (fdatasync
//! before the watermark advances) → demote → cold-read every record
//! through **real executor suspension** (`IoOp::TierRead` + `IoGate`) →
//! seeded copy-to-tail updates promote a subset → RAM hits verify.
//!
//! Then the S06 crash leg: a seeded **power cut** tears the un-fsynced
//! disk state, and recovery replays the op ledger (the harness plays the
//! WAL, exactly as the store-level oracle in
//! `inf-store/tests/tiered_mutation.rs` does) into a **new life** at a
//! fresh origin — flush, demote, and cold-read everything again. The
//! oracle compares content and versions, never addresses (§3.1
//! "addresses are per-life"). No record lost, none duplicated.
//!
//! Every event folds into `trace_hash`; `--verify-determinism` runs the
//! scenario twice and requires hash identity (L7).

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use inf_alloc::{AlignedBufId, AlignedPool, BufferPool};
use inf_foundation::hash64;
use inf_foundation::rng::{Entropy, SplitMix64};
use inf_log::fs::sim::SimDisk;
use inf_log::{
    TIER_FRAME_BYTES, TIER_FRAME_DATA, TierWriter, tier_extract, tier_frame_offset, tier_frame_span,
};
use inf_runtime::{
    BackendDriver, CellExecutor, CompletionResult, CompletionToken, GateWait, IoGate, IoOp,
    PollImmediate, RawFd, StableBytesMut, TokenClass, Wait,
};
use inf_store::{
    AddrClass, AddressSpaceConfig, Keyspace, LogicalAddr, NsId, StoreConfig, TieredLookup,
    TieredTable,
};

use crate::net::{CellNet, Plant, SimDriver};

const NS: NsId = NsId(77);
const RING: usize = 1 << 20;
const PAGE: usize = 1 << 12;
const POOL_BUF: usize = 4 * TIER_FRAME_BYTES;

/// Scenario knobs — the DSL v0 shape (a struct, not a language).
#[derive(Debug)]
pub struct SteelScenario {
    pub seed: u64,
    /// Records in the corpus.
    pub records: u64,
    /// Copy-to-tail promotions after demotion.
    pub updates: u64,
}

impl SteelScenario {
    #[must_use]
    pub fn m4_steel(seed: u64) -> SteelScenario {
        SteelScenario { seed, records: 200, updates: 40 }
    }
}

#[derive(Debug, Default)]
pub struct SteelReport {
    pub violations: Vec<String>,
    pub cold_reads: u64,
    pub promotions: u64,
    pub trace_hash: u64,
}

impl SteelReport {
    #[must_use]
    pub fn ok(&self) -> bool {
        self.violations.is_empty()
    }
}

/// One key's expected state (the model) plus the op ledger entry shape.
struct Expect {
    value: Vec<u8>,
    version: u32,
}

struct Ctx {
    ks: Keyspace,
    pool: AlignedPool,
    driver: SimDriver,
    gate: IoGate,
    tier_fd: RawFd,
    tier_base: u64,
    tier_frames: u64,
    next_token: u32,
}

impl Ctx {
    fn table(&mut self) -> &mut TieredTable {
        self.ks.tiered_store_mut(NS).expect("materialized")
    }

    fn mint_token(&mut self) -> CompletionToken {
        let token = CompletionToken::new(TokenClass::TierRead, self.next_token, 0);
        self.next_token += 1;
        token
    }
}

type Waiter = GateWait<CompletionToken, CompletionResult>;
type Answer = Option<(Vec<u8>, u32, bool)>;

enum Planned {
    Done(Answer),
    Fetch { buf: AlignedBufId, addr: LogicalAddr, frame_count: usize, skip: usize, waiter: Waiter },
}

/// Phase 1 — resolve + plan under one short borrow (the sim pool window
/// always covers a whole record: corpus values stay well below it, so
/// the two-round oversized leg stays with the Linux twin).
fn plan(ctx: &Rc<RefCell<Ctx>>, key: &[u8], hash: u64, exclude: &[LogicalAddr]) -> Planned {
    let ctx = &mut *ctx.borrow_mut();
    match ctx.table().lookup(key, hash, exclude) {
        TieredLookup::Ram(addr) => {
            let parts = ctx.table().record(addr);
            Planned::Done(Some((parts.value.to_vec(), parts.version, false)))
        }
        TieredLookup::Miss => Planned::Done(None),
        TieredLookup::Cold(addr) => {
            let delta = addr.to_raw() - ctx.tier_base;
            let (first_frame, _, skip) = tier_frame_span(delta, TieredTable::RECORD_HEADER_LEN);
            let window_frames = (ctx.pool.buf_size() / TIER_FRAME_BYTES) as u64;
            let frame_count = window_frames.min(ctx.tier_frames - first_frame) as usize;
            let buf = ctx.pool.try_lease().expect("scenario pool is sized");
            let token = ctx.mint_token();
            let dest = &mut ctx.pool.bytes_mut(buf)[..frame_count * TIER_FRAME_BYTES];
            // SAFETY: the pool buffer's address is stable for the pool's
            // lifetime and the lease is held (untouched) until this op's
            // terminal completion resumes us.
            let stable = unsafe { StableBytesMut::new(dest) };
            ctx.driver.push(IoOp::TierRead {
                fd: ctx.tier_fd,
                offset: tier_frame_offset(first_frame),
                buf: stable,
                token,
            });
            let waiter = ctx.gate.waiter(token);
            Planned::Fetch { buf, addr, frame_count, skip, waiter }
        }
    }
}

/// The scenario GET — fetch-verify-retry over real suspension, resuming
/// with a re-resolve (the M0 custody rule).
async fn steel_get(ctx: Rc<RefCell<Ctx>>, key: Vec<u8>) -> Answer {
    let hash = TieredTable::hash_key(&key);
    let mut exclude: Vec<LogicalAddr> = Vec::new();
    loop {
        let (buf, addr, frame_count, skip, waiter) = match plan(&ctx, &key, hash, &exclude) {
            Planned::Done(answer) => return answer,
            Planned::Fetch { buf, addr, frame_count, skip, waiter } => {
                (buf, addr, frame_count, skip, waiter)
            }
        };
        let result = waiter.await; // ← the suspension: plain data only
        let ctx_ref = &mut *ctx.borrow_mut();
        assert!(matches!(result, CompletionResult::TierRead), "cold read failed: {result:?}");
        match ctx_ref.table().lookup(&key, hash, &exclude) {
            TieredLookup::Ram(promoted) => {
                ctx_ref.pool.release(buf);
                let parts = ctx_ref.table().record(promoted);
                return Some((parts.value.to_vec(), parts.version, false));
            }
            TieredLookup::Miss => {
                ctx_ref.pool.release(buf);
                return None;
            }
            TieredLookup::Cold(now) => {
                assert_eq!(now, addr, "scenario mutates nothing mid-read");
                let window = &ctx_ref.pool.bytes(buf)[..frame_count * TIER_FRAME_BYTES];
                let mut head = Vec::new();
                tier_extract(window, skip, TieredTable::RECORD_HEADER_LEN, &mut head)
                    .expect("header frames verify");
                let len = TieredTable::record_len_from_header(&head);
                assert!(
                    skip + len <= frame_count * TIER_FRAME_DATA,
                    "scenario records fit one window (the oversized leg is the Linux twin's)"
                );
                let mut record = Vec::new();
                tier_extract(window, skip, len, &mut record).expect("record frames verify");
                ctx_ref.pool.release(buf);
                let parts = TieredTable::decode_record(&record);
                if parts.key == key {
                    return Some((parts.value.to_vec(), parts.version, true));
                }
                exclude.push(addr); // 2⁻²² fingerprint false positive
            }
        }
    }
}

/// The reactor's SUBMIT+REAP and EXECUTE steps, played deterministically.
fn pump_until<T>(
    ctx: &Rc<RefCell<Ctx>>,
    ex: &mut CellExecutor,
    done: &Rc<RefCell<Option<T>>>,
) -> T {
    let mut recv_pool = BufferPool::new(2, 4096);
    for _ in 0..64 {
        if let Some(answer) = done.borrow_mut().take() {
            return answer;
        }
        let mut out = Vec::new();
        {
            let ctx = &mut *ctx.borrow_mut();
            ctx.driver.submit_and_reap(&mut recv_pool, Wait::Poll, &mut out).expect("submit");
        }
        {
            let ctx = ctx.borrow();
            for completion in out {
                assert_eq!(completion.token.class(), TokenClass::TierRead);
                ctx.gate.complete(completion.token, completion.result);
            }
        }
        ex.run_ready(64);
    }
    panic!("m4-steel pump exceeded its bound");
}

fn run_get(ctx: &Rc<RefCell<Ctx>>, ex: &mut CellExecutor, key: &[u8]) -> (Answer, bool) {
    let done: Rc<RefCell<Option<Answer>>> = Rc::new(RefCell::new(None));
    let sink = Rc::clone(&done);
    let fut_ctx = Rc::clone(ctx);
    let key = key.to_vec();
    let outcome = ex.poll_immediate(async move {
        let answer = steel_get(fut_ctx, key).await;
        *sink.borrow_mut() = Some(answer);
    });
    match outcome {
        PollImmediate::Completed => {
            (done.borrow_mut().take().expect("completed future stored its answer"), false)
        }
        PollImmediate::Suspended(_) => (pump_until(ctx, ex, &done), true),
    }
}

/// The promotion leg's inputs: the seeded rng, how many updates, and the
/// ledger the updates append to (the "WAL" the crash leg replays).
type UpdateLeg<'a> = (&'a mut SplitMix64, u64, &'a mut Vec<(Vec<u8>, Vec<u8>)>);

/// One life: build the table from the op ledger, seal + flush + demote,
/// cold-read everything against the model, then apply `updates` seeded
/// copy-to-tail promotions (recorded into the ledger for the next life).
#[allow(clippy::too_many_arguments)]
fn run_life(
    disk: &SimDisk,
    shard_dir: &std::path::Path,
    tier_file_id: u32,
    life_origin: LogicalAddr,
    ledger: &[(Vec<u8>, Vec<u8>)],
    updates_from: Option<UpdateLeg<'_>>,
    report: &mut SteelReport,
) -> LogicalAddr {
    // Replay the ledger into a fresh life (the harness plays the WAL).
    let mut ks = Keyspace::new(StoreConfig::default());
    assert!(ks.materialize_tiered(
        NS,
        AddressSpaceConfig { reserve_bytes: RING, page_bytes: PAGE, life_origin },
        64,
    ));
    let mut model: std::collections::BTreeMap<Vec<u8>, Expect> = std::collections::BTreeMap::new();
    let mut log: Vec<(LogicalAddr, usize)> = Vec::new();
    {
        let table = ks.tiered_store_mut(NS).expect("materialized");
        for (key, value) in ledger {
            let hash = TieredTable::hash_key(key);
            let (placed, fresh_alloc) = match table.lookup(key, hash, &[]) {
                TieredLookup::Ram(old) => {
                    let (old_len, old_version) = {
                        let parts = table.record(old);
                        (parts.encoded_len, parts.version)
                    };
                    let placed =
                        table.update(key, value, hash, old, old_len, old_version).expect("fits");
                    (placed, placed != old) // in place ⇒ already in the log
                }
                TieredLookup::Miss => (table.insert(key, value, hash).expect("fits"), true),
                TieredLookup::Cold(_) => unreachable!("nothing is cold during replay"),
            };
            let parts = table.record(placed);
            model.insert(key.clone(), Expect { value: value.clone(), version: parts.version });
            if fresh_alloc {
                log.push((placed, parts.encoded_len));
            }
        }
    }
    // Seal → flush (fdatasync on the sim disk) → demote.
    let table = ks.tiered_store_mut(NS).expect("materialized");
    let tail = table.space().tail();
    table.space_mut().advance_ro_boundary(tail);
    let base = table.space().head();
    let mut writer =
        TierWriter::create(disk, shard_dir, tier_file_id, 0, NS, base).expect("tier file");
    // The file is a byte range, not a record list: every allocated range
    // appends in address order — dead copies (replay relocations) flush
    // as the raw bytes they still occupy, keeping the range contiguous.
    for (addr, len) in &log {
        let bytes = table.record_bytes(*addr, *len).to_vec();
        writer.append(*addr, &bytes).expect("append");
    }
    writer.sync().expect("fdatasync before the watermark advances");
    table.space_mut().advance_flushed(tail);
    table.space_mut().advance_head(tail);

    let tier_frames = writer.data_len().div_ceil(TIER_FRAME_DATA as u64);
    let ctx = Rc::new(RefCell::new(Ctx {
        ks,
        pool: AlignedPool::new(2, POOL_BUF),
        driver: SimDriver::with_disk(CellNet::new(0, 0xD15C_0000, Plant::None), disk.clone()),
        gate: IoGate::new(),
        tier_fd: writer.raw_fd().expect("sim files carry fake fds"),
        tier_base: base.to_raw(),
        tier_frames,
        next_token: 1,
    }));
    let mut ex = CellExecutor::new(8);

    // Cold-read every model key; content + version compared (never
    // addresses — §3.1).
    let keys: Vec<Vec<u8>> = model.keys().cloned().collect();
    for key in &keys {
        let (answer, suspended) = run_get(&ctx, &mut ex, key);
        let want = model.get(key).expect("model");
        match answer {
            Some((value, version, served_cold)) => {
                if value != want.value {
                    report
                        .violations
                        .push(format!("value mismatch for {}", String::from_utf8_lossy(key)));
                }
                if version != want.version {
                    report
                        .violations
                        .push(format!("version mismatch for {}", String::from_utf8_lossy(key)));
                }
                if !served_cold || !suspended {
                    report
                        .violations
                        .push(format!("demoted key served warm: {}", String::from_utf8_lossy(key)));
                }
                report.cold_reads += 1;
                report.trace_hash = hash64(&value, report.trace_hash ^ u64::from(version));
            }
            None => {
                report.violations.push(format!("lost record: {}", String::from_utf8_lossy(key)))
            }
        }
    }

    // Seeded copy-to-tail promotions (recorded into the ledger for the
    // crash leg — they are the ops the "WAL" carries forward).
    if let Some((rng, updates, ledger_out)) = updates_from {
        for _ in 0..updates {
            let key = keys[(rng.next_u64() % keys.len() as u64) as usize].clone();
            let value_len = 20 + (rng.next_u64() % 80) as usize;
            let value = vec![(rng.next_u64() % 251) as u8; value_len];
            let ctx = &mut *ctx.borrow_mut();
            let table = ctx.table();
            let hash = TieredTable::hash_key(&key);
            let TieredLookup::Cold(old) = table.lookup(&key, hash, &[]) else {
                // Already promoted by an earlier update this round: RAM.
                let TieredLookup::Ram(old) = table.lookup(&key, hash, &[]) else {
                    report.violations.push("update lost its key".into());
                    continue;
                };
                let (old_len, old_version) = {
                    let parts = table.record(old);
                    (parts.encoded_len, parts.version)
                };
                table.update(&key, &value, hash, old, old_len, old_version).expect("fits");
                ledger_out.push((key, value));
                continue;
            };
            let want = model.get(&key).expect("model");
            let old_len = TieredTable::RECORD_HEADER_LEN + key.len() + want.value.len();
            let placed =
                table.update(&key, &value, hash, old, old_len, want.version).expect("fits");
            if table.space().resolve(placed) != AddrClass::Mutable {
                report.violations.push("promotion did not land in the mutable region".into());
            }
            report.promotions += 1;
            ledger_out.push((key, value));
        }
        // Promoted keys now serve from RAM without suspension.
        let ledger_tail: Vec<Vec<u8>> =
            ledger_out.iter().rev().take(updates as usize).map(|(k, _)| k.clone()).collect();
        for key in ledger_tail {
            let (answer, suspended) = run_get(&ctx, &mut ex, &key);
            match answer {
                Some((value, _, served_cold)) => {
                    if served_cold || suspended {
                        report.violations.push("promoted key still served cold".into());
                    }
                    report.trace_hash = hash64(&value, report.trace_hash);
                }
                None => report.violations.push("promoted key lost".into()),
            }
        }
    }

    let ctx = ctx.borrow();
    if ctx.pool.reconcile().is_err() {
        report.violations.push("aligned-pool lease leak".into());
    }
    tail
}

/// Runs the scenario once. Deterministic from `scenario.seed` (L7).
#[must_use]
pub fn run_steel_scenario(scenario: &SteelScenario) -> SteelReport {
    let mut report = SteelReport::default();
    let mut rng = SplitMix64::new(scenario.seed ^ 0x57EE_1000);
    let disk = SimDisk::new();
    let shard_dir = PathBuf::from("node/shard-0");

    // The op ledger — the harness-WAL both lives replay from.
    let mut ledger: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    for i in 0..scenario.records {
        let key = format!("steel:{i:06}").into_bytes();
        let value_len = 16 + (rng.next_u64() % 120) as usize;
        let value = vec![(rng.next_u64() % 251) as u8; value_len];
        ledger.push((key, value));
    }

    // Life 1: write → flush → demote → cold reads → promotions.
    let tail = run_life(
        &disk,
        &shard_dir,
        0,
        LogicalAddr::ZERO,
        &ledger.clone(),
        Some((&mut rng, scenario.updates, &mut ledger)),
        &mut report,
    );

    // The S06 crash leg: tear un-fsynced state, then a new life replays
    // the ledger at a fresh origin — content survives, addresses don't.
    disk.power_cut(scenario.seed ^ 0x0FF5_EED0);
    let origin = LogicalAddr::from_raw(tail.to_raw().next_multiple_of(PAGE as u64))
        .expect("origin fits 48 bits");
    run_life(&disk, &shard_dir, 1, origin, &ledger, None, &mut report);
    report
}
