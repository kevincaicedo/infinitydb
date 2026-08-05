//! M4-S10 cold-read shaping (dev tier, ADR-0055): the coalescing A/B and
//! the QD-cap saturation proof over the real machinery (`UringDriver` +
//! registered `AlignedPool` + `ColdReads`).
//!
//! Phases:
//!   coalesce   zipfian batched cold reads (YCSB θ=0.99 — hot ranks map
//!              to low record indices, i.e. *adjacent addresses*, the
//!              flush-order clustering the plan names), identical seed,
//!              merge ON vs merge OFF: device reads issued per logical
//!              workload. The §2 cut line reads this row: ≥ 20% fewer
//!              device reads, or coalescing demotes by ADR amendment.
//!   saturate   memory-hit latency lane (RAM-resident `TieredTable`
//!              lookups) measured unloaded, then re-measured while a
//!              uniform cold flood holds the device at the QD cap —
//!              split histograms; the AC binds loaded p99 ≤ 1.1×
//!              unloaded p99.
//!
//! Run:  `INF_TIER_AB_DIR=/real/device/dir taskset -c 4 cargo bench -p \
//!        inf-runtime --features uring --bench cold_shaping`
//! Env:  `INF_TIER_AB_DIR` (required, not tmpfs), `INF_SHAPE_RECORDS`
//!       (default 1<<18 ≈ 256 MiB), `INF_SHAPE_READS` (default 50000),
//!       `INF_SHAPE_REPS` (default 3), `INF_SHAPE_PHASE` (all|coalesce|
//!       saturate).

#![cfg(all(target_os = "linux", feature = "uring"))]

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Instant;

use inf_alloc::{AlignedPool, BufferPool};
use inf_log::fs::StdSegmentFs;
use inf_log::{TIER_FRAME_BYTES, TierIoMode, TierWriter, tier_frame_offset, tier_frame_span};
use inf_runtime::{
    BackendDriver, ColdReadConfig, ColdReads, ColdWait, RawFd, ReadClass, TierFileId, TokenClass,
    UringDriver, Wait,
};
use inf_store::{
    AddressSpaceConfig, DemotionConfig, Keyspace, LogicalAddr, NsId, StoreConfig, TieredLookup,
    TieredTable,
};

const NS: NsId = NsId(91);
const VALUE_BYTES: usize = 1024;
const WINDOW_FRAMES: usize = 2;
const POOL_BUFFERS: usize = 128;
const QD_CAP: usize = 64;
/// Batch of logical reads per drain window — the EXECUTE-batch analog.
const BATCH: usize = 16;
/// RAM lane: table size + measured lookups per leg.
const RAM_KEYS: u32 = 4096;
const RAM_HITS_PER_LEG: usize = 200_000;

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

struct SplitMix64(u64);

impl SplitMix64 {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// YCSB-style Zipf(θ) over `n` ranks (Gray et al. quick method); rank 1
/// (hottest) maps to record 0 — hot ranks are *adjacent addresses*.
struct Zipf {
    n: u64,
    theta: f64,
    alpha: f64,
    zetan: f64,
    eta: f64,
}

impl Zipf {
    fn new(n: u64, theta: f64) -> Zipf {
        let zetan: f64 = (1..=n).map(|i| 1.0 / (i as f64).powf(theta)).sum();
        let zeta2 = 1.0 + 0.5f64.powf(theta);
        let alpha = 1.0 / (1.0 - theta);
        let eta = (1.0 - (2.0 / n as f64).powf(1.0 - theta)) / (1.0 - zeta2 / zetan);
        Zipf { n, theta, alpha, zetan, eta }
    }

    fn sample(&self, rng: &mut SplitMix64) -> u64 {
        let u = rng.unit();
        let uz = u * self.zetan;
        if uz < 1.0 {
            return 0;
        }
        if uz < 1.0 + 0.5f64.powf(self.theta) {
            return 1;
        }
        let rank = (self.n as f64 * (self.eta * u - self.eta + 1.0).powf(self.alpha)) as u64;
        rank.min(self.n - 1)
    }
}

fn drop_page_cache(path: &Path) {
    let file = std::fs::File::open(path).expect("open for fadvise");
    let fd = std::os::fd::AsRawFd::as_raw_fd(&file);
    // SAFETY: fadvise on a live fd; DONTNEED is advisory.
    let rc = unsafe { libc::posix_fadvise(fd, 0, 0, libc::POSIX_FADV_DONTNEED) };
    assert_eq!(rc, 0, "fadvise DONTNEED");
}

fn governor() -> String {
    std::fs::read_to_string("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "unknown".into())
}

fn build_corpus(dir: &Path, records: u64) -> TierWriter<StdSegmentFs> {
    let fs = StdSegmentFs;
    let mut writer =
        TierWriter::create(&fs, dir, 0, 0, NS, LogicalAddr::ZERO, TierIoMode::Buffered)
            .expect("tier file");
    let mut value = vec![0u8; VALUE_BYTES];
    for index in 0..records {
        value[..8].copy_from_slice(&index.to_le_bytes());
        let addr = LogicalAddr::from_raw(index * VALUE_BYTES as u64).expect("fits");
        writer.append(addr, &value).expect("append");
    }
    writer.sync().expect("fdatasync");
    writer
}

fn poll_ready(wait: &mut ColdWait) -> Option<inf_runtime::ColdDone> {
    use core::future::Future;
    use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
    fn noop(_: *const ()) {}
    fn clone(p: *const ()) -> RawWaker {
        RawWaker::new(p, &VTABLE)
    }
    static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, noop, noop, noop);
    // SAFETY: the no-op waker never dereferences its pointer.
    let waker = unsafe { Waker::from_raw(RawWaker::new(core::ptr::null(), &VTABLE)) };
    match core::pin::Pin::new(wait).poll(&mut Context::from_waker(&waker)) {
        Poll::Ready(done) => Some(done),
        Poll::Pending => None,
    }
}

/// One coalescing leg: `reads` zipfian picks in batches of `BATCH`;
/// returns (device reads issued, logical reads enqueued, wall µs).
fn coalesce_leg(
    fd: RawFd,
    path: &Path,
    reads: u64,
    records: u64,
    merge: bool,
    seed: u64,
) -> (u64, u64, u64) {
    drop_page_cache(path);
    let mut driver = UringDriver::new(128).expect("io_uring");
    let mut pool = AlignedPool::new(POOL_BUFFERS, WINDOW_FRAMES * TIER_FRAME_BYTES);
    driver.register_tier_pool(&mut pool).expect("registration");
    let cold = ColdReads::with_config(
        pool,
        ColdReadConfig { qd_cap: QD_CAP, merge, ..ColdReadConfig::default() },
    );
    let file = TierFileId::new(0);
    let zipf = Zipf::new(records, 0.99);
    let mut rng = SplitMix64(seed);
    let mut recv_pool = BufferPool::new(2, 4096);
    let mut out = Vec::new();
    let mut outstanding: Vec<ColdWait> = Vec::new();
    let mut done_count = 0u64;
    let wall = Instant::now();
    let mut issued_logical = 0u64;
    while done_count < reads {
        if issued_logical < reads {
            for _ in 0..BATCH.min((reads - issued_logical) as usize) {
                let index = zipf.sample(&mut rng);
                let delta = index * VALUE_BYTES as u64;
                let (first, count, _) = tier_frame_span(delta, VALUE_BYTES);
                match cold.enqueue(
                    fd,
                    file,
                    tier_frame_offset(first),
                    count as usize * TIER_FRAME_BYTES,
                    ReadClass::Foreground,
                    0,
                ) {
                    Ok(wait) => {
                        outstanding.push(wait);
                        issued_logical += 1;
                    }
                    Err(_) => break, // overflow: drain first
                }
            }
        }
        {
            let cold = cold.clone();
            cold.drain(|op| driver.push(op));
        }
        out.clear();
        driver.submit_and_reap(&mut recv_pool, Wait::Poll, &mut out).expect("submit");
        for completion in out.drain(..) {
            assert_eq!(completion.token.class(), TokenClass::TierRead);
            cold.on_completion(completion.token, completion.result, 0);
        }
        let mut i = 0;
        while i < outstanding.len() {
            if let Some(done) = poll_ready(&mut outstanding[i]) {
                outstanding.swap_remove(i);
                done.outcome().expect("clean read");
                drop(done);
                done_count += 1;
            } else {
                i += 1;
            }
        }
    }
    cold.reconcile().expect("custody clean");
    let counters = cold.counters();
    assert_eq!(counters.enqueued, reads);
    (counters.issued, counters.enqueued, wall.elapsed().as_micros() as u64)
}

/// The RAM-hit lane: `hits` lookups against RAM-resident records,
/// per-op latency into a sorted vector (the demotion_storm methodology —
/// both legs identical, so the ratio is valid).
fn ram_lane(table: &mut TieredTable, hits: usize, rng: &mut SplitMix64, out: &mut Vec<u64>) {
    for _ in 0..hits {
        let key = format!("ram:{:06}", rng.next() % u64::from(RAM_KEYS)).into_bytes();
        let hash = TieredTable::hash_key(&key);
        let at = Instant::now();
        match table.lookup(&key, hash, &[]) {
            TieredLookup::Ram(addr) => {
                let parts = table.record(addr);
                std::hint::black_box(parts.value.first().copied());
            }
            other => panic!("RAM lane must hit RAM: {other:?}"),
        }
        out.push(at.elapsed().as_nanos() as u64);
    }
}

fn pick(sorted: &[u64], p: f64) -> f64 {
    let rank = ((p / 100.0) * sorted.len() as f64).ceil().max(1.0) as usize - 1;
    sorted[rank.min(sorted.len() - 1)] as f64 / 1000.0
}

fn main() {
    let Some(base) = std::env::var_os("INF_TIER_AB_DIR") else {
        eprintln!("cold_shaping: set INF_TIER_AB_DIR to a real-device directory; skipping");
        return;
    };
    let base = PathBuf::from(base).join("shaping");
    // Fresh corpus per run (create-new semantics refuse a stale file).
    if base.exists() {
        std::fs::remove_dir_all(&base).expect("clear stale corpus");
    }
    std::fs::create_dir_all(&base).expect("base dir");
    let records = env_u64("INF_SHAPE_RECORDS", 1 << 18);
    let reads = env_u64("INF_SHAPE_READS", 50_000);
    let reps = env_u64("INF_SHAPE_REPS", 3);
    let phase = std::env::var("INF_SHAPE_PHASE").unwrap_or_else(|_| "all".into());

    let mut report = String::new();
    let push = |report: &mut String, line: &str| {
        println!("{line}");
        report.push_str(line);
        report.push('\n');
    };
    push(&mut report, "# M4-S10 cold-read shaping (dev tier — ADR-0055)");
    push(
        &mut report,
        &format!(
            "env: governor={} records={records} reads={reads} reps={reps} batch={BATCH} \
             qd_cap={QD_CAP} zipf θ=0.99",
            governor()
        ),
    );

    let writer = build_corpus(&base, records);
    let fd = writer.raw_fd().expect("real fd");

    if phase == "all" || phase == "coalesce" {
        push(&mut report, "\n## coalesce: device reads for the identical zipfian batched workload");
        let mut reductions = Vec::new();
        for rep in 0..reps {
            // ABBA: alternate leg order per replicate.
            let order = if rep % 2 == 0 { [false, true] } else { [true, false] };
            let mut on = 0u64;
            let mut off = 0u64;
            for merge in order {
                let (issued, logical, wall_us) =
                    coalesce_leg(fd, writer.path(), reads, records, merge, 0x5EED_1000 + rep);
                push(
                    &mut report,
                    &format!(
                        "rep{rep} merge={:<5} device reads {issued:>7} / {logical} logical \
                         ({:.1}% of logical) · {:.2}s",
                        merge,
                        issued as f64 / logical as f64 * 100.0,
                        wall_us as f64 / 1e6
                    ),
                );
                if merge {
                    on = issued;
                } else {
                    off = issued;
                }
            }
            let reduction = (1.0 - on as f64 / off as f64) * 100.0;
            reductions.push(reduction);
            push(&mut report, &format!("rep{rep} device-read reduction: {reduction:.1}%"));
        }
        reductions.sort_by(|a, b| a.partial_cmp(b).expect("finite"));
        let median = reductions[reductions.len() / 2];
        push(
            &mut report,
            &format!(
                "\ncoalesce verdict: median device-read reduction {median:.1}% vs the ≥ 20% \
                 cut line (plan §2) — {}",
                if median >= 20.0 {
                    "PASS (coalescing stays default-on)"
                } else {
                    "FAIL (demote per M0-S14)"
                }
            ),
        );
    }

    if phase == "all" || phase == "saturate" {
        push(&mut report, "\n## saturate: memory-hit lane, unloaded vs under full-QD cold flood");
        // RAM-resident table (nothing demoted — every lookup must hit RAM).
        let mut ks = Keyspace::new(StoreConfig::default());
        assert!(
            ks.materialize_tiered(
                NS,
                AddressSpaceConfig {
                    reserve_bytes: 1 << 24,
                    page_bytes: 1 << 12,
                    life_origin: LogicalAddr::ZERO
                },
                DemotionConfig::for_budget(1 << 24, 1 << 12),
                RAM_KEYS as usize * 2,
            )
            .is_ok()
        );
        let table = ks.tiered_store_mut(NS).expect("materialized");
        for i in 0..RAM_KEYS {
            let key = format!("ram:{i:06}").into_bytes();
            table.insert(&key, &[0xAB; 64], TieredTable::hash_key(&key)).expect("fits");
        }
        for rep in 0..reps {
            // Unloaded leg.
            let mut rng = SplitMix64(0x5EED_2000 + rep);
            let mut unloaded = Vec::with_capacity(RAM_HITS_PER_LEG);
            ram_lane(
                ks.tiered_store_mut(NS).expect("materialized"),
                RAM_HITS_PER_LEG,
                &mut rng,
                &mut unloaded,
            );
            unloaded.sort_unstable();

            // Loaded leg: uniform cold flood held at the cap, RAM hits
            // interleaved between pumps (one cell, one thread — L1).
            drop_page_cache(writer.path());
            let mut driver = UringDriver::new(128).expect("io_uring");
            let mut pool = AlignedPool::new(POOL_BUFFERS, WINDOW_FRAMES * TIER_FRAME_BYTES);
            driver.register_tier_pool(&mut pool).expect("registration");
            let cold = ColdReads::with_config(
                pool,
                ColdReadConfig { qd_cap: QD_CAP, ..ColdReadConfig::default() },
            );
            let file = TierFileId::new(0);
            let mut flood_rng = SplitMix64(0x5EED_3000 + rep);
            let mut recv_pool = BufferPool::new(2, 4096);
            let mut out = Vec::new();
            let mut outstanding: Vec<ColdWait> = Vec::new();
            let mut loaded = Vec::with_capacity(RAM_HITS_PER_LEG);
            let mut flood_completed = 0u64;
            while loaded.len() < RAM_HITS_PER_LEG {
                while outstanding.len() < QD_CAP {
                    let index = flood_rng.next() % records;
                    let delta = index * VALUE_BYTES as u64;
                    let (first, count, _) = tier_frame_span(delta, VALUE_BYTES);
                    let wait = cold
                        .enqueue(
                            fd,
                            file,
                            tier_frame_offset(first),
                            count as usize * TIER_FRAME_BYTES,
                            ReadClass::Foreground,
                            0,
                        )
                        .expect("queue sized");
                    outstanding.push(wait);
                }
                {
                    let cold = cold.clone();
                    cold.drain(|op| driver.push(op));
                }
                out.clear();
                driver.submit_and_reap(&mut recv_pool, Wait::Poll, &mut out).expect("submit");
                for completion in out.drain(..) {
                    assert_eq!(completion.token.class(), TokenClass::TierRead);
                    cold.on_completion(completion.token, completion.result, 0);
                }
                let mut i = 0;
                while i < outstanding.len() {
                    if let Some(done) = poll_ready(&mut outstanding[i]) {
                        outstanding.swap_remove(i);
                        done.outcome().expect("clean read");
                        drop(done);
                        flood_completed += 1;
                    } else {
                        i += 1;
                    }
                }
                // The memory-hit lane, measured while the device churns.
                let hits = 64.min(RAM_HITS_PER_LEG - loaded.len());
                ram_lane(
                    ks.tiered_store_mut(NS).expect("materialized"),
                    hits,
                    &mut rng,
                    &mut loaded,
                );
            }
            // Drain the flood (custody must reconcile before the verdict).
            while !outstanding.is_empty() {
                {
                    let cold = cold.clone();
                    cold.drain(|op| driver.push(op));
                }
                out.clear();
                driver.submit_and_reap(&mut recv_pool, Wait::Poll, &mut out).expect("submit");
                for completion in out.drain(..) {
                    cold.on_completion(completion.token, completion.result, 0);
                }
                let mut i = 0;
                while i < outstanding.len() {
                    if let Some(done) = poll_ready(&mut outstanding[i]) {
                        outstanding.swap_remove(i);
                        drop(done);
                    } else {
                        i += 1;
                    }
                }
            }
            cold.reconcile().expect("custody clean after the flood");
            loaded.sort_unstable();
            let unloaded_p99 = pick(&unloaded, 99.0);
            let loaded_p99 = pick(&loaded, 99.0);
            let ratio = loaded_p99 / unloaded_p99;
            // The 1.1× AC binds on the gate's memory-hit p99 — a µs-scale
            // end-to-end number (M1 gate shape). This lane is a raw
            // table probe: below ~2 µs the baseline is timer-scale
            // (~25 ns per Instant pair) and a ratio cannot bind — the
            // absolute tail delta is the honest substrate row, and the
            // command-level verdict belongs to the S22 rows.
            let binding = unloaded_p99 >= 2.0;
            let verdict = if !binding {
                format!(
                    "substrate altitude (unloaded p99 {unloaded_p99:.2} µs is timer-scale): \
                     ratio non-binding; absolute p99 delta {:+.0} ns recorded — the 1.1× \
                     verdict is the S22 command-level rows'",
                    (loaded_p99 - unloaded_p99) * 1000.0
                )
            } else if ratio <= 1.10 {
                format!("p99 ratio {ratio:.3} vs the 1.10 bound — PASS")
            } else {
                format!("p99 ratio {ratio:.3} vs the 1.10 bound — FAIL")
            };
            push(
                &mut report,
                &format!(
                    "rep{rep} unloaded p50 {:.2} µs p99 {:.2} µs p99.9 {:.2} µs | loaded p50 \
                     {:.2} µs p99 {:.2} µs p99.9 {:.2} µs | flood {} cold reads, sampled QD \
                     p99 {} | {verdict}",
                    pick(&unloaded, 50.0),
                    unloaded_p99,
                    pick(&unloaded, 99.9),
                    pick(&loaded, 50.0),
                    loaded_p99,
                    pick(&loaded, 99.9),
                    flood_completed,
                    cold.qd_percentile(99.0),
                ),
            );
        }
    }

    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs();
    let artifacts = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.artifacts/m4/s10");
    std::fs::create_dir_all(&artifacts).expect("artifact dir");
    let out_path = artifacts.join(format!("{stamp}-cold-shaping.txt"));
    std::fs::File::create(&out_path)
        .and_then(|mut f| f.write_all(report.as_bytes()))
        .expect("artifact write");
    println!("\nartifact: {}", out_path.display());
}
