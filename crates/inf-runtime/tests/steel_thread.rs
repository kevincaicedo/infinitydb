//! M4-S04 steel thread: the smallest honest end-to-end vertical of the
//! riskiest path — write → seal → flush (tier file, fdatasync) → demote
//! (pages released) → **cold read through executor suspension** (real
//! io_uring `TierRead`, the `IoGate` seam M0 built) → update after
//! demotion (copy-to-tail promotes, M4-S06) → RAM hit.
//!
//! The test plays the plane: it owns the keyspace-materialized tiered
//! table, the tier writer, the aligned pool, and the driver pump, and
//! drives the M4 §3.1 lifecycle explicitly. Custody rules bind exactly
//! as in production (the M0 lesson): no borrow crosses an await — a
//! suspended GET holds only plain data (a pool lease id, an address,
//! frame indices) and **re-resolves after resume**.
//!
//! The deterministic twin of this scenario is `inf-sim`'s `m4-steel`.

#![cfg(all(target_os = "linux", feature = "uring"))]

use std::cell::RefCell;
use std::rc::Rc;

use inf_alloc::{AlignedBox, AlignedBufId, AlignedPool, BufferPool};
use inf_log::fs::StdSegmentFs;
use inf_log::{
    TIER_FRAME_BYTES, TIER_FRAME_DATA, TierWriter, tier_extract, tier_frame_offset, tier_frame_span,
};
use inf_runtime::{
    BackendDriver, CellExecutor, CompletionResult, CompletionToken, GateWait, IoGate, IoOp,
    PollImmediate, RawFd, StableBytesMut, TokenClass, UringDriver, Wait,
};
use inf_store::{
    AddrClass, AddressSpaceConfig, Keyspace, LogicalAddr, NsId, StoreConfig, TieredLookup,
    TieredTable,
};

const NS: NsId = NsId(42);
const RING: usize = 1 << 21; // 2 MiB — fits the corpus incl. the big record
const PAGE: usize = 1 << 12;
/// Pool window: 16 frames per first read; the big record overruns it and
/// exercises the exact second read.
const POOL_BUF: usize = 16 * TIER_FRAME_BYTES;

type Waiter = GateWait<CompletionToken, CompletionResult>;
type Answer = Option<(Vec<u8>, u32, bool)>;

struct Ctx {
    ks: Keyspace,
    pool: AlignedPool,
    driver: UringDriver,
    gate: IoGate,
    tier_fd: RawFd,
    /// First logical address the tier file covers.
    tier_base: u64,
    /// Frames durably in the file (reads must never overrun them).
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

/// First-window plan — everything the suspended GET carries is plain data.
struct WindowPlan {
    buf: AlignedBufId,
    addr: LogicalAddr,
    frame_count: usize,
    skip: usize,
}

enum Planned {
    Done(Answer),
    Fetch(WindowPlan, Waiter),
}

enum Resumed {
    Answer(Answer),
    /// 2⁻²² fingerprint false positive: retry with the address excluded.
    FalsePositive(LogicalAddr),
    /// Record overran the window: exact second read is in flight.
    Oversized {
        boxed: AlignedBox,
        waiter: Waiter,
        addr: LogicalAddr,
        skip: usize,
        len: usize,
    },
}

/// Phase 1 — resolve + plan under one short borrow.
fn plan_first_window(
    ctx: &Rc<RefCell<Ctx>>,
    key: &[u8],
    hash: u64,
    exclude: &[LogicalAddr],
) -> Planned {
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
            let buf = ctx.pool.try_lease().expect("steel-thread pool is sized");
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
            Planned::Fetch(WindowPlan { buf, addr, frame_count, skip }, waiter)
        }
    }
}

/// Phase 3 — resume the first window: **re-resolve before trusting
/// anything** (the M0 custody rule), then decode, retry, or escalate to
/// the exact oversized read. One short borrow.
fn resume_first_window(
    ctx: &Rc<RefCell<Ctx>>,
    key: &[u8],
    hash: u64,
    exclude: &[LogicalAddr],
    plan: WindowPlan,
    result: CompletionResult,
) -> Resumed {
    let ctx = &mut *ctx.borrow_mut();
    assert!(matches!(result, CompletionResult::TierRead), "cold read failed: {result:?}");
    match ctx.table().lookup(key, hash, exclude) {
        TieredLookup::Ram(addr) => {
            // Promoted while we slept: RAM wins, the window is stale.
            ctx.pool.release(plan.buf);
            let parts = ctx.table().record(addr);
            Resumed::Answer(Some((parts.value.to_vec(), parts.version, false)))
        }
        TieredLookup::Miss => {
            ctx.pool.release(plan.buf);
            Resumed::Answer(None)
        }
        TieredLookup::Cold(addr) => {
            assert_eq!(addr, plan.addr, "steel thread mutates nothing mid-read");
            let window = &ctx.pool.bytes(plan.buf)[..plan.frame_count * TIER_FRAME_BYTES];
            let mut head = Vec::new();
            tier_extract(window, plan.skip, TieredTable::RECORD_HEADER_LEN, &mut head)
                .expect("header frames verify");
            let len = TieredTable::record_len_from_header(&head);
            if plan.skip + len <= plan.frame_count * TIER_FRAME_DATA {
                let mut record = Vec::new();
                tier_extract(window, plan.skip, len, &mut record).expect("record frames verify");
                ctx.pool.release(plan.buf);
                let parts = TieredTable::decode_record(&record);
                if parts.key == key {
                    return Resumed::Answer(Some((parts.value.to_vec(), parts.version, true)));
                }
                return Resumed::FalsePositive(addr);
            }
            // Oversized: exact frame range into a one-off aligned box
            // (S08's bounded chunked staging replaces this class).
            ctx.pool.release(plan.buf);
            let delta = addr.to_raw() - ctx.tier_base;
            let (first, count, skip) = tier_frame_span(delta, len);
            let mut boxed = AlignedBox::new(count as usize * TIER_FRAME_BYTES);
            let token = ctx.mint_token();
            // SAFETY: the box's heap allocation is address-stable while
            // owned; it is neither read nor dropped until this op's
            // terminal completion (this future is never cancelled — the
            // S08 registered pool owns the general answer).
            let stable = unsafe { StableBytesMut::new(boxed.bytes_mut()) };
            ctx.driver.push(IoOp::TierRead {
                fd: ctx.tier_fd,
                offset: tier_frame_offset(first),
                buf: stable,
                token,
            });
            let waiter = ctx.gate.waiter(token);
            Resumed::Oversized { boxed, waiter, addr, skip, len }
        }
    }
}

/// Phase 5 — resume the oversized read: re-resolve again, decode from the
/// owned box. One short borrow. (The argument list is the suspended
/// state, spelled out — test-local plumbing, not an API.)
#[allow(clippy::too_many_arguments)]
fn resume_oversized(
    ctx: &Rc<RefCell<Ctx>>,
    key: &[u8],
    hash: u64,
    exclude: &[LogicalAddr],
    addr: LogicalAddr,
    boxed: &AlignedBox,
    skip: usize,
    len: usize,
    result: CompletionResult,
) -> Resumed {
    let ctx = &mut *ctx.borrow_mut();
    assert!(matches!(result, CompletionResult::TierRead), "oversized read failed: {result:?}");
    match ctx.table().lookup(key, hash, exclude) {
        TieredLookup::Ram(promoted) => {
            let parts = ctx.table().record(promoted);
            Resumed::Answer(Some((parts.value.to_vec(), parts.version, false)))
        }
        TieredLookup::Miss => Resumed::Answer(None),
        TieredLookup::Cold(now) => {
            assert_eq!(now, addr, "steel thread mutates nothing mid-read");
            let mut record = Vec::new();
            tier_extract(boxed.bytes(), skip, len, &mut record).expect("record frames verify");
            let parts = TieredTable::decode_record(&record);
            if parts.key == key {
                return Resumed::Answer(Some((parts.value.to_vec(), parts.version, true)));
            }
            Resumed::FalsePositive(addr)
        }
    }
}

/// The steel-thread GET (the L6 shape): RAM answers synchronously; cold
/// candidates fetch-verify-retry through real suspension.
async fn steel_get(ctx: Rc<RefCell<Ctx>>, key: Vec<u8>) -> Answer {
    let hash = TieredTable::hash_key(&key);
    let mut exclude: Vec<LogicalAddr> = Vec::new();
    loop {
        let (plan, waiter) = match plan_first_window(&ctx, &key, hash, &exclude) {
            Planned::Done(answer) => return answer,
            Planned::Fetch(plan, waiter) => (plan, waiter),
        };
        let result = waiter.await; // ← suspension #1: plain data only
        match resume_first_window(&ctx, &key, hash, &exclude, plan, result) {
            Resumed::Answer(answer) => return answer,
            Resumed::FalsePositive(addr) => {
                exclude.push(addr);
                continue;
            }
            Resumed::Oversized { boxed, waiter, addr, skip, len } => {
                let result = waiter.await; // ← suspension #2: owned box + indices
                match resume_oversized(&ctx, &key, hash, &exclude, addr, &boxed, skip, len, result)
                {
                    Resumed::Answer(answer) => return answer,
                    Resumed::FalsePositive(addr) => {
                        exclude.push(addr);
                        continue;
                    }
                    Resumed::Oversized { .. } => unreachable!("second read is exact"),
                }
            }
        }
    }
}

/// The reactor's SUBMIT+REAP and EXECUTE steps, played by the test: pump
/// the driver, deliver completions to the gate, poll ready tasks —
/// bounded, never spinning past the answer.
fn pump_until<T>(
    ctx: &Rc<RefCell<Ctx>>,
    ex: &mut CellExecutor,
    done: &Rc<RefCell<Option<T>>>,
) -> T {
    let mut recv_pool = BufferPool::new(2, 4096); // signature-required; unused by TierRead
    for _ in 0..1000 {
        if let Some(answer) = done.borrow_mut().take() {
            return answer;
        }
        let mut out = Vec::new();
        {
            let ctx = &mut *ctx.borrow_mut();
            ctx.driver
                .submit_and_reap(
                    &mut recv_pool,
                    Wait::Park { timeout: Some(std::time::Duration::from_millis(5)) },
                    &mut out,
                )
                .expect("submit");
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
    panic!("steel-thread pump exceeded its bound");
}

/// Runs one GET to completion through the executor, asserting it truly
/// suspended when `expect_suspend` says so.
fn run_get(
    ctx: &Rc<RefCell<Ctx>>,
    ex: &mut CellExecutor,
    key: &[u8],
    expect_suspend: bool,
) -> Answer {
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
            assert!(!expect_suspend, "expected a suspension (cold read), got a fast path");
            done.borrow_mut().take().expect("completed future stored its answer")
        }
        PollImmediate::Suspended(_) => {
            assert!(expect_suspend, "expected the fast path, got a suspension");
            pump_until(ctx, ex, &done)
        }
    }
}

struct Corpus {
    small_keys: Vec<Vec<u8>>,
    big_key: Vec<u8>,
    values: std::collections::HashMap<Vec<u8>, Vec<u8>>,
}

/// Builds the table, seals + flushes + demotes everything, and returns
/// the corpus — the §3.1 lifecycle up to "disk-only".
fn build_demoted_corpus(
    dir: &std::path::Path,
) -> (Rc<RefCell<Ctx>>, Corpus, TierWriter<StdSegmentFs>) {
    let mut ks = Keyspace::new(StoreConfig::default());
    assert!(ks.materialize_tiered(
        NS,
        AddressSpaceConfig {
            reserve_bytes: RING,
            page_bytes: PAGE,
            life_origin: LogicalAddr::ZERO
        },
        64,
    ));
    assert_eq!(ks.tiered_tables(), 1, "the S04 landing-site entry exists");

    let mut corpus = Corpus {
        small_keys: (0..32u32).map(|i| format!("steel:{i:04}").into_bytes()).collect(),
        big_key: b"steel:big".to_vec(),
        values: std::collections::HashMap::new(),
    };
    let mut log: Vec<(LogicalAddr, usize)> = Vec::new();
    {
        let table = ks.tiered_store_mut(NS).expect("materialized");
        for (i, key) in corpus.small_keys.iter().enumerate() {
            let value = vec![b'a' + (i % 23) as u8; 40 + i * 7];
            let addr = table.insert(key, &value, TieredTable::hash_key(key)).expect("fits");
            log.push((addr, table.record(addr).encoded_len));
            corpus.values.insert(key.clone(), value);
        }
        // The big record: > one pool window (16 frames ≈ 64 KiB) forces
        // the exact second read.
        let big_value = vec![0xB6u8; 100 * 1024];
        let addr = table
            .insert(&corpus.big_key, &big_value, TieredTable::hash_key(&corpus.big_key))
            .expect("fits");
        log.push((addr, table.record(addr).encoded_len));
        corpus.values.insert(corpus.big_key.clone(), big_value);
    }

    // Seal → flush (tier file + fdatasync) → demote (release pages).
    let fs = StdSegmentFs;
    let table = ks.tiered_store_mut(NS).expect("materialized");
    let tail = table.space().tail();
    table.space_mut().advance_ro_boundary(tail);
    let base = table.space().head();
    let mut writer = TierWriter::create(&fs, dir, 0, 0, NS, base).expect("tier file");
    for (addr, len) in &log {
        let bytes = table.record_bytes(*addr, *len).to_vec();
        writer.append(*addr, &bytes).expect("append");
    }
    writer.sync().expect("fdatasync before the watermark advances");
    assert_eq!(writer.durable_len(), tail.to_raw() - base.to_raw());
    table.space_mut().advance_flushed(tail);
    table.space_mut().advance_head(tail);
    // Whole pages below the head are released; only the partial tail page
    // may stay committed (new allocations continue on it).
    assert!(
        table.space().report().committed_bytes <= PAGE as u64,
        "demotion released the RAM pages"
    );

    let tier_frames = writer.data_len().div_ceil(TIER_FRAME_DATA as u64);
    let ctx = Rc::new(RefCell::new(Ctx {
        ks,
        pool: AlignedPool::new(4, POOL_BUF),
        driver: UringDriver::new(64).expect("io_uring"),
        gate: IoGate::new(),
        tier_fd: writer.raw_fd().expect("std fs has real fds"),
        tier_base: base.to_raw(),
        tier_frames,
        next_token: 1,
    }));
    (ctx, corpus, writer)
}

/// The M4-S04 AC: the full lifecycle, including update-after-demotion.
#[test]
fn steel_thread_write_flush_demote_cold_read() {
    let dir = std::env::temp_dir().join(format!("inf-steel-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp shard dir");
    let (ctx, corpus, _writer) = build_demoted_corpus(&dir);
    let mut ex = CellExecutor::new(8);

    // Cold reads: every record served byte-exact through real suspension.
    for key in &corpus.small_keys {
        let (value, _, served_cold) =
            run_get(&ctx, &mut ex, key, true).expect("demoted key resolves");
        assert_eq!(&value, corpus.values.get(key).expect("corpus"), "byte-exact cold read");
        assert!(served_cold, "below head must serve from the tier file");
    }
    // The oversized record exercises the exact second read.
    let (value, _, served_cold) =
        run_get(&ctx, &mut ex, &corpus.big_key, true).expect("big key resolves");
    assert_eq!(value, corpus.values[&corpus.big_key], "oversized two-round read is byte-exact");
    assert!(served_cold);

    // A missing key never touches the disk (index answers Miss).
    assert!(run_get(&ctx, &mut ex, b"steel:absent", false).is_none());

    // Update after demotion: copy-to-tail promotes (M4-S06) — the cold
    // copy becomes dead bytes, the record is hot again.
    let hot_key = corpus.small_keys[7].clone();
    {
        let ctx = &mut *ctx.borrow_mut();
        let table = ctx.table();
        let hash = TieredTable::hash_key(&hot_key);
        let TieredLookup::Cold(old) = table.lookup(&hot_key, hash, &[]) else {
            panic!("expected the demoted record to be cold")
        };
        let old_len =
            corpus.values[&hot_key].len() + TieredTable::RECORD_HEADER_LEN + hot_key.len();
        let dead_before = table.space().report().dead_bytes;
        let placed = table
            .update(&hot_key, b"promoted-by-update", hash, old, old_len, 0)
            .expect("copy-to-tail fits");
        assert_eq!(table.space().resolve(placed), AddrClass::Mutable, "promoted to the tail");
        assert_eq!(
            table.space().report().dead_bytes,
            dead_before + old_len as u64,
            "the cold copy is dead bytes at the repoint moment"
        );
    }
    // The promoted record now serves from RAM — no suspension.
    let (value, version, served_cold) =
        run_get(&ctx, &mut ex, &hot_key, false).expect("promoted key resolves");
    assert_eq!(value, b"promoted-by-update");
    assert_eq!(version, 1, "copy-to-tail bumped the version");
    assert!(!served_cold, "update-after-demotion promotes to RAM");

    // Custody reconciles: every aligned lease came back.
    let ctx = ctx.borrow();
    assert_eq!(ctx.pool.reconcile(), Ok(()));
    let counters = ctx.ks.tiering_counters();
    assert!(counters.cold_resolves > 0, "the cold path provably executed");
    std::fs::remove_dir_all(&dir).ok();
}

/// Cold-read latency histogram (informational, risk-gate input — L10:
/// never quotable as a claim). Run explicitly:
/// `INF_STEEL_DIR=<dir-on-nvme> cargo test -p inf-runtime --features uring --release -- --ignored cold_read_histogram --nocapture`
/// `INF_STEEL_DIR` must sit on the device under test (temp_dir is often
/// tmpfs — a RAM histogram would be a lie); page cache is dropped per
/// read via `posix_fadvise(DONTNEED)` so the number reflects the device.
#[test]
#[ignore = "histogram harness — run explicitly for the .artifacts/m4/s04 artifact"]
fn cold_read_histogram() {
    let dir = std::env::var_os("INF_STEEL_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join(format!("inf-steel-hist-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp shard dir");
    let (ctx, corpus, _writer) = build_demoted_corpus(&dir);
    let mut ex = CellExecutor::new(8);

    let fd = ctx.borrow().tier_fd;
    let mut lat_us: Vec<f64> = Vec::new();
    let rounds = 300usize;
    for round in 0..rounds {
        let key = &corpus.small_keys[round % corpus.small_keys.len()];
        // Drop the file's page cache so every read hits the device.
        // SAFETY: fadvise on a live owned fd with a zero range (whole file).
        let rc = unsafe { libc::posix_fadvise(fd, 0, 0, libc::POSIX_FADV_DONTNEED) };
        assert_eq!(rc, 0, "fadvise");
        let t = std::time::Instant::now();
        let (_, _, served_cold) = run_get(&ctx, &mut ex, key, true).expect("resolves");
        let elapsed = t.elapsed();
        assert!(served_cold);
        lat_us.push(elapsed.as_secs_f64() * 1e6);
    }
    lat_us.sort_by(f64::total_cmp);
    let pct = |p: f64| lat_us[((lat_us.len() as f64 * p) as usize).min(lat_us.len() - 1)];
    println!("--- M4-S04 steel-thread cold-read histogram (prototype, informational) ---");
    println!("rounds: {rounds} (fadvise-DONTNEED per read; includes pump overhead)");
    println!(
        "p50 {:.1} us | p90 {:.1} us | p99 {:.1} us | max {:.1} us",
        pct(0.50),
        pct(0.90),
        pct(0.99),
        pct(1.0)
    );
    std::fs::remove_dir_all(&dir).ok();
}
