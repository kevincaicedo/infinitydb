//! M4-S08 hardened cold reads on real io_uring: the `ColdReads` custody
//! path (registered aligned pool, per-file pins, chunked staging,
//! cancellation) over actual tier files — the production-shaped
//! successor of the S04 steel thread.
//!
//! What this proves that the steel thread could not:
//!
//! - **Hot commands proceed during a cold read** (L6): while a cold GET
//!   is suspended on its in-flight NVMe read, RAM lookups and writes on
//!   the same table complete — asserted *before* the cold completion is
//!   pumped.
//! - **Bounded chunked staging**: a record larger than a pool buffer
//!   reads through multiple pool-window issues (never an oversized
//!   one-off allocation); peak staging is one pool lease per in-flight
//!   read, asserted via the pool's lease count.
//! - **Cancellation custody**: waiters dropped before and after their
//!   completion release the buffer and unpin the file through the
//!   `ColdDone` guard — `reconcile()` proves zero leaks on the real
//!   backend, not just the sim.
//! - **Pin-deferred unlink**: a file with an in-flight read is not
//!   unlinked; after the read drains, the unlink proceeds and the fd
//!   observes it (the §3.3 rule on a real filesystem).

#![cfg(all(target_os = "linux", feature = "uring"))]

use std::cell::RefCell;
use std::rc::Rc;

use inf_alloc::{AlignedPool, BufferPool};
use inf_log::fs::StdSegmentFs;
use inf_log::{
    TIER_FRAME_BYTES, TIER_FRAME_DATA, TierIoMode, TierWriter, tier_extract, tier_frame_offset,
    tier_frame_span,
};
use inf_runtime::{
    BackendDriver, CellExecutor, ColdReads, ColdWait, PollImmediate, RawFd, ReadClass, TierFileId,
    TokenClass, UringDriver, Wait,
};
use inf_store::KeyHasher;
use inf_store::{
    AddressSpaceConfig, DemotionConfig, Keyspace, LogicalAddr, NsId, StoreConfig, TieredLookup,
    TieredTable,
};

const NS: NsId = NsId(43);
const RING: usize = 1 << 21;
const PAGE: usize = 1 << 12;
/// Pool: 4 buffers × 4 frames — the 100 KiB record stages through ~7
/// chunk reads.
const POOL_BUFFERS: usize = 4;
const POOL_BUF: usize = 4 * TIER_FRAME_BYTES;

struct Ctx {
    ks: Keyspace,
    cold: ColdReads,
    driver: UringDriver,
    tier_fd: RawFd,
    tier_file: TierFileId,
    tier_base: u64,
    tier_frames: u64,
}

impl Ctx {
    fn table(&mut self) -> &mut TieredTable {
        self.ks.tiered_store_mut(NS).expect("materialized")
    }
}

type Answer = Option<(Vec<u8>, u32, bool)>;

/// The hardened GET: fetch-verify-retry through `ColdReads`, re-resolve
/// after every resume, chunked staging for oversized records.
async fn hardened_get(ctx: Rc<RefCell<Ctx>>, key: Vec<u8>) -> Answer {
    let hash = KeyHasher::default().hash(&key);
    let mut exclude: Vec<LogicalAddr> = Vec::new();
    'attempt: loop {
        let (addr, wait, window_frames, skip) = {
            let ctx = &mut *ctx.borrow_mut();
            match ctx.table().lookup(&key, hash, &exclude) {
                TieredLookup::Ram(addr) => {
                    let parts = ctx.table().record(addr);
                    return Some((parts.value.to_vec(), parts.version, false));
                }
                TieredLookup::Miss => return None,
                TieredLookup::Cold(addr) => {
                    let delta = addr.to_raw() - ctx.tier_base;
                    let (first, _, skip) = tier_frame_span(delta, TieredTable::RECORD_HEADER_LEN);
                    let window_frames =
                        ((POOL_BUF / TIER_FRAME_BYTES) as u64).min(ctx.tier_frames - first);
                    let len = window_frames as usize * TIER_FRAME_BYTES;
                    let wait = ctx
                        .cold
                        .enqueue(
                            ctx.tier_fd,
                            ctx.tier_file,
                            tier_frame_offset(first),
                            len,
                            ReadClass::Foreground,
                            0,
                        )
                        .expect("test queue is sized for its concurrency");
                    (addr, wait, window_frames, skip)
                }
            }
        };
        let done = wait.await; // ← suspension: plain data + the waiter only
        done.outcome().expect("clean read");
        enum After {
            Serve(Answer),
            Retry,
            Stage { total: usize, assembled: Vec<u8> },
        }
        let after = {
            let ctx = &mut *ctx.borrow_mut();
            match ctx.table().lookup(&key, hash, &exclude) {
                TieredLookup::Ram(promoted) => {
                    let parts = ctx.table().record(promoted);
                    After::Serve(Some((parts.value.to_vec(), parts.version, false)))
                }
                TieredLookup::Miss => After::Serve(None),
                TieredLookup::Cold(now) => {
                    assert_eq!(now, addr, "this test relocates nothing mid-read");
                    done.bytes(|window| {
                        let mut head = Vec::new();
                        tier_extract(window, skip, TieredTable::RECORD_HEADER_LEN, &mut head)
                            .expect("header frames verify");
                        let total = TieredTable::record_len_from_header(&head);
                        let window_data = window_frames as usize * TIER_FRAME_DATA;
                        if skip + total <= window_data {
                            let mut record = Vec::new();
                            tier_extract(window, skip, total, &mut record)
                                .expect("record frames verify");
                            let parts = TieredTable::decode_record(&record);
                            if parts.key == key {
                                After::Serve(Some((parts.value.to_vec(), parts.version, true)))
                            } else {
                                After::Retry
                            }
                        } else {
                            let take = window_data - skip;
                            let mut assembled = Vec::with_capacity(total);
                            tier_extract(window, skip, take, &mut assembled)
                                .expect("first-window frames verify");
                            After::Stage { total, assembled }
                        }
                    })
                }
            }
        };
        drop(done); // custody home before any further await
        match after {
            After::Serve(answer) => return answer,
            After::Retry => {
                exclude.push(addr);
                continue;
            }
            After::Stage { total, mut assembled } => {
                while assembled.len() < total {
                    let remaining = total - assembled.len();
                    let (wait, window_frames) = {
                        let ctx = &mut *ctx.borrow_mut();
                        // Peak staging: exactly one pool lease per
                        // in-flight read of this GET.
                        let delta = addr.to_raw() - ctx.tier_base + assembled.len() as u64;
                        let (first, _, chunk_skip) = tier_frame_span(delta, remaining);
                        assert_eq!(chunk_skip, 0, "continuation chunks are frame-aligned");
                        let window_frames =
                            ((POOL_BUF / TIER_FRAME_BYTES) as u64).min(ctx.tier_frames - first);
                        let len = window_frames as usize * TIER_FRAME_BYTES;
                        let wait = ctx
                            .cold
                            .enqueue(
                                ctx.tier_fd,
                                ctx.tier_file,
                                tier_frame_offset(first),
                                len,
                                ReadClass::Foreground,
                                0,
                            )
                            .expect("staging queues one window at a time");
                        (wait, window_frames)
                    };
                    let done = wait.await;
                    done.outcome().expect("clean staging read");
                    let ctx = &mut *ctx.borrow_mut();
                    match ctx.table().lookup(&key, hash, &exclude) {
                        TieredLookup::Cold(now) if now == addr => done.bytes(|window| {
                            let take = remaining.min(window_frames as usize * TIER_FRAME_DATA);
                            let mut piece = Vec::new();
                            tier_extract(window, 0, take, &mut piece)
                                .expect("staging frames verify");
                            assembled.extend_from_slice(&piece);
                        }),
                        other => panic!("this test relocates nothing mid-read: {other:?}"),
                    }
                }
                let parts = TieredTable::decode_record(&assembled);
                if parts.key == key {
                    return Some((parts.value.to_vec(), parts.version, true));
                }
                exclude.push(addr);
                continue 'attempt;
            }
        }
    }
}

/// Pump: submit + reap, routing TierRead completions into the custody
/// table; then run resumed futures.
fn pump(ctx: &Rc<RefCell<Ctx>>, ex: &mut CellExecutor) {
    let mut recv_pool = BufferPool::new(2, 4096);
    let mut out = Vec::new();
    {
        let ctx = &mut *ctx.borrow_mut();
        // S10: the drain admits queued intents (merging + QD cap) into
        // the same submit the eager path always rode.
        let cold = ctx.cold.clone();
        cold.drain(|op| ctx.driver.push(op));
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
            ctx.cold.on_completion(completion.token, completion.result, 0);
        }
    }
    ex.run_ready(64);
}

fn pump_until<T>(
    ctx: &Rc<RefCell<Ctx>>,
    ex: &mut CellExecutor,
    done: &Rc<RefCell<Option<T>>>,
) -> T {
    for _ in 0..1000 {
        if let Some(answer) = done.borrow_mut().take() {
            return answer;
        }
        pump(ctx, ex);
    }
    panic!("cold-hardened pump exceeded its bound");
}

struct Corpus {
    small_keys: Vec<Vec<u8>>,
    big_key: Vec<u8>,
    values: std::collections::HashMap<Vec<u8>, Vec<u8>>,
}

/// Build → seal → flush → demote, with the registered pool + custody
/// path wired (the S04 corpus shape over the S08 machinery).
fn build_demoted_corpus(
    dir: &std::path::Path,
) -> (Rc<RefCell<Ctx>>, Corpus, TierWriter<StdSegmentFs>) {
    let mut ks = Keyspace::new(StoreConfig::default());
    assert!(
        ks.materialize_tiered(
            NS,
            AddressSpaceConfig {
                reserve_bytes: RING,
                page_bytes: PAGE,
                life_origin: LogicalAddr::ZERO
            },
            DemotionConfig::for_budget(RING as u64, PAGE as u64),
            64,
        )
        .is_ok()
    );
    let mut corpus = Corpus {
        small_keys: (0..24u32).map(|i| format!("hard:{i:04}").into_bytes()).collect(),
        big_key: b"hard:big".to_vec(),
        values: std::collections::HashMap::new(),
    };
    let mut log: Vec<(LogicalAddr, usize)> = Vec::new();
    {
        let table = ks.tiered_store_mut(NS).expect("materialized");
        for (i, key) in corpus.small_keys.iter().enumerate() {
            let value = vec![b'a' + (i % 23) as u8; 48 + i * 5];
            let addr = table.insert(key, &value, KeyHasher::default().hash(key)).expect("fits");
            log.push((addr, table.record(addr).encoded_len));
            corpus.values.insert(key.clone(), value);
        }
        // > POOL_BUF (16 KiB) by a wide margin: ~7 staged chunks.
        let big_value = vec![0xB8u8; 100 * 1024];
        let addr = table
            .insert(&corpus.big_key, &big_value, KeyHasher::default().hash(&corpus.big_key))
            .expect("fits");
        log.push((addr, table.record(addr).encoded_len));
        corpus.values.insert(corpus.big_key.clone(), big_value);
    }
    let fs = StdSegmentFs;
    let table = ks.tiered_store_mut(NS).expect("materialized");
    let tail = table.space().tail();
    table.space_mut().advance_ro_boundary(tail);
    let base = table.space().head();
    let mut writer =
        TierWriter::create(&fs, dir, 0, 0, NS, base, TierIoMode::Buffered).expect("tier file");
    for (addr, len) in &log {
        let bytes = table.record_bytes(*addr, *len).to_vec();
        writer.append(*addr, &bytes).expect("append");
    }
    writer.sync().expect("fdatasync before the watermark advances");
    table.space_mut().advance_flushed(tail);
    table.space_mut().advance_head(tail);

    let mut driver = UringDriver::new(64).expect("io_uring");
    // The registered pool (M4-S08): fixed-buffer registration; reads on
    // pool buffers upgrade to the fixed opcode transparently.
    let mut pool = AlignedPool::new(POOL_BUFFERS, POOL_BUF);
    driver.register_tier_pool(&mut pool).expect("registration never fails boot");
    let tier_frames = writer.data_len().div_ceil(TIER_FRAME_DATA as u64);
    let ctx = Rc::new(RefCell::new(Ctx {
        ks,
        cold: ColdReads::new(pool),
        driver,
        tier_fd: writer.raw_fd().expect("std fs has real fds"),
        tier_file: TierFileId::new(0),
        tier_base: base.to_raw(),
        tier_frames,
    }));
    (ctx, corpus, writer)
}

fn spawn_get(
    ctx: &Rc<RefCell<Ctx>>,
    ex: &mut CellExecutor,
    key: &[u8],
) -> (Rc<RefCell<Option<Answer>>>, bool) {
    let done: Rc<RefCell<Option<Answer>>> = Rc::new(RefCell::new(None));
    let sink = Rc::clone(&done);
    let fut_ctx = Rc::clone(ctx);
    let key = key.to_vec();
    let outcome = ex.poll_immediate(async move {
        let answer = hardened_get(fut_ctx, key).await;
        *sink.borrow_mut() = Some(answer);
    });
    (done, matches!(outcome, PollImmediate::Suspended(_)))
}

/// Hot-commands-proceed + chunked staging + custody, one vertical.
#[test]
fn cold_reads_share_the_cell_with_hot_commands() {
    let dir = std::env::temp_dir().join(format!("inf-cold-hardened-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp shard dir");
    let (ctx, corpus, _writer) = build_demoted_corpus(&dir);
    let mut ex = CellExecutor::new(16);

    // Issue a cold GET; it suspends with its intent queued (S10), and
    // the drain step admits it to the device.
    let cold_key = corpus.small_keys[3].clone();
    let (cold_done, suspended) = spawn_get(&ctx, &mut ex, &cold_key);
    assert!(suspended, "a demoted key must suspend");
    assert!(cold_done.borrow().is_none(), "the cold read is still queued");
    {
        let ctx = &mut *ctx.borrow_mut();
        assert_eq!(ctx.cold.queue_depth(), 1, "the suspension parked an intent");
        let cold = ctx.cold.clone();
        assert_eq!(cold.drain(|op| ctx.driver.push(op)), 1, "the drain admits it");
        assert_eq!(ctx.cold.inflight_total(), 1, "one device read in flight");
        assert_eq!(ctx.cold.inflight_on(TierFileId::new(0)), 1, "the tier file is pinned");
    }

    // The L6 point: hot commands complete on the same cell BEFORE the
    // cold completion is pumped.
    {
        let ctx = &mut *ctx.borrow_mut();
        let hot_key = b"hot:while-cold";
        let hash = KeyHasher::default().hash(hot_key);
        let addr = ctx.table().insert(hot_key, b"written-during-cold-read", hash).expect("fits");
        assert_eq!(ctx.table().record(addr).value, b"written-during-cold-read");
        let (ram_answer, ram_suspended) = {
            let _ = &addr;
            (ctx.table().lookup(hot_key, hash, &[]), false)
        };
        assert!(matches!(ram_answer, TieredLookup::Ram(_)), "hot lookup answers instantly");
        assert!(!ram_suspended);
        assert!(cold_done.borrow().is_none(), "cold read STILL in flight while hot ops ran");
    }

    // Now let the cold read complete.
    let answer = pump_until(&ctx, &mut ex, &cold_done);
    let (value, _, served_cold) = answer.expect("demoted key resolves");
    assert_eq!(&value, corpus.values.get(&cold_key).expect("corpus"));
    assert!(served_cold);

    // Chunked staging: the 100 KiB record needs multiple pool windows.
    let issued_before = ctx.borrow().cold.counters().issued;
    let (big_done, suspended) = spawn_get(&ctx, &mut ex, &corpus.big_key);
    assert!(suspended);
    let answer = pump_until(&ctx, &mut ex, &big_done);
    let (value, _, served_cold) = answer.expect("big key resolves");
    assert_eq!(value, corpus.values[&corpus.big_key], "staged read is byte-exact");
    assert!(served_cold);
    let issued = ctx.borrow().cold.counters().issued - issued_before;
    assert!(issued >= 7, "100 KiB through 16 KiB windows needs ≥ 7 chunk reads, got {issued}");

    // Every lease, pin, and in-flight entry reconciles.
    assert_eq!(ctx.borrow().cold.reconcile(), Ok(()));
    std::fs::remove_dir_all(&dir).ok();
}

/// Cancellation custody on the real backend: waiters dropped mid-flight
/// and post-delivery both release through the guard; the pin defers a
/// real unlink until the drain.
#[test]
fn cancellation_and_unlink_discipline_on_uring() {
    let dir = std::env::temp_dir().join(format!("inf-cold-cancel-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp shard dir");
    let (ctx, corpus, writer) = build_demoted_corpus(&dir);
    let mut ex = CellExecutor::new(16);
    let file = TierFileId::new(0);

    // Leg 1 — cancel before completion: issue a raw read, drop the
    // waiter, pump. The unclaimed completion must release custody.
    {
        let ctx_ref = &mut *ctx.borrow_mut();
        let (first, _, _) = tier_frame_span(0, TieredTable::RECORD_HEADER_LEN);
        let wait: ColdWait = ctx_ref
            .cold
            .enqueue(
                ctx_ref.tier_fd,
                file,
                tier_frame_offset(first),
                POOL_BUF,
                ReadClass::Foreground,
                0,
            )
            .expect("queue sized");
        let cold = ctx_ref.cold.clone();
        assert_eq!(cold.drain(|op| ctx_ref.driver.push(op)), 1, "the read went in flight");
        drop(wait); // the client disconnected AFTER the issue — mid-flight
        assert_eq!(ctx_ref.cold.inflight_on(file), 1, "the op still owns buffer + pin");
    }
    // The pin defers the unlink while the read is in flight (§3.3).
    assert!(ctx.borrow().cold.inflight_on(file) > 0, "unlink must wait for the drain");
    for _ in 0..100 {
        pump(&ctx, &mut ex);
        if ctx.borrow().cold.inflight_on(file) == 0 {
            break;
        }
    }
    assert_eq!(ctx.borrow().cold.inflight_on(file), 0, "unclaimed completion unpinned");
    assert_eq!(ctx.borrow().cold.counters().unclaimed, 1);
    assert_eq!(ctx.borrow().cold.reconcile(), Ok(()), "cancelled read leaked nothing");

    // Leg 2 — the drain observed, the unlink proceeds on the real fs.
    std::fs::remove_file(writer.path()).expect("unlink after drain");

    // Reads already in flight against the unlinked-but-open fd still
    // complete (POSIX keeps the inode; the pin held the fd open) — the
    // EBADF class §3.3 forbids cannot occur.
    let (done, suspended) = spawn_get(&ctx, &mut ex, &corpus.small_keys[1]);
    assert!(suspended, "cold read against the still-open fd");
    let answer = pump_until(&ctx, &mut ex, &done);
    let (value, _, served_cold) = answer.expect("resolves from the open fd");
    assert_eq!(&value, corpus.values.get(&corpus.small_keys[1]).expect("corpus"));
    assert!(served_cold);
    assert_eq!(ctx.borrow().cold.reconcile(), Ok(()));
    std::fs::remove_dir_all(&dir).ok();
}
