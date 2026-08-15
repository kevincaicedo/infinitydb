//! `inf-bench mixed-audit` — the M4-S20 mixed-node coexistence audit
//! (master plan §4.3 `unified` profile: one node, several products).
//!
//! One durable-booted node hosts three namespaces at once: a cache
//! namespace (`MODE memory`, `EVICTION allkeys-lru`, bounded
//! `MAXMEMORY`), the default document store (M3 JSON corpus), and a
//! durable-**tiered** namespace created with real budgets. Each workload
//! runs **solo** first (the baselines), then the cache + document legs
//! run **concurrently** while a sampler thread watches RSS and the L5
//! attribution domains continuously — the audit is the honesty artifact:
//! per-namespace isolation deltas against same-campaign baselines, the
//! `sum(domains) vs RSS` divergence throughout the mixed run, and a §19
//! generator-saturation disposition per leg.
//!
//! **The tiered data leg runs (since 2026-08-15, S24 phase 5).** M4-S26
//! lifted the ADR-0062 D8 `USE` refusal, so the third workload is real:
//! a uniform 1:1 SET/GET stream over a dataset 10× the tiered
//! namespace's memory budget, deeply pipelined so the *cold-read queue*
//! is what the other two namespaces must coexist with — the AC's
//! "full-QD cold reads" condition. The refusal probe is inverted rather
//! than deleted: the audit now fails loudly if the plane is **not**
//! reachable, so this leg can never silently become named-absent again.
//! Topics join at M7, collections at M5 — both still named in the
//! report.
//!
//! Generator sizing is deliberate (the C5 lesson): the cache and
//! document legs keep the exact connection counts their solo baselines
//! use, and the tiered leg buys its queue depth from *pipelining* rather
//! than from threads — 2 connections that are blocked on cold-read I/O
//! essentially all the time. Cold-read queue depth is a server-side
//! property of outstanding requests, not of generator threads, so this
//! reaches full QD without putting the mixed leg's generator on a
//! different footing from the solo legs'.

use std::io::Read as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use inf_foundation::LogHistogram;

use crate::cli::Flags;
use crate::gaterun::{ServerGuard, scrape_cells, spawn_infinityd, sum_field};
use crate::load::{LoadReport, LoadSpec, render, run as run_load};
use crate::m2rows::delta_pct;
use crate::resp::{connect, encode_command, reply_len, request};

const DOC_NS: &str = "audit-doc";
const TIER_NS: &str = "audit-tier";
/// Generator sizing rule: the **mixed** total (cache + doc conns + the
/// sampler) must fit the loadgen cpu set, or the mixed leg measures
/// generator crowding instead of server interference — the C5 lesson
/// applied to this harness's own threads. 8 + 4 fits the 12-cpu
/// loadgen set this box uses; solo legs run the identical config so
/// the generator side is constant across the comparison.
const CACHE_CONNS: usize = 8;
const CACHE_PIPELINE: usize = 16;
const DOC_CONNS: usize = 4;
/// Node-level maxmemory (the proven M1 eviction machinery — Redis
/// semantics, divided per cell). Sized as documents' static share plus a
/// cache share the offered working set overruns, so eviction is active.
/// **Audit finding (recorded in the report):** per-namespace `MAXMEMORY`
/// on named memory namespaces is registry-carried but unenforced — the
/// eviction sweep rotates the numbered dbs only — so the cache leg runs
/// on the default DB, where the machinery is real.
const NODE_MAXMEMORY: &str = "96mb";
const CACHE_KEYS: u64 = 262_144;
const CACHE_VALUE: usize = 512;
/// Document corpus: per-index-unique 1 KiB documents (the gate shape).
const DOC_SHAPE: &str = "gate-1KiB";
const DOC_KEYS: u64 = 20_000;
/// The tiered leg (S24 phase 5). `TIER_KEYS × TIER_VALUE` is **10×**
/// `TIER_MEM_BUDGET`, the §7 shape: 1,310,720 × 512 B = 640 MiB against a
/// 64 MiB budget. Uniform keys, not zipfian — this leg exists to be the
/// *interference source* for the other two namespaces, and uniform over
/// 10× RAM is the maximal-cold-read shape (a zipfian hot set would serve
/// most reads from memory and understate the interference the AC asks
/// about). The hot-set question itself belongs to `inf-bench ycsb`.
const TIER_CONNS: usize = 2;
/// Queue depth comes from pipelining, not threads (see the module doc).
const TIER_PIPELINE: usize = 32;
const TIER_VALUE: usize = 512;
const TIER_KEYS: u64 = 1_310_720;
const TIER_MEM_BUDGET: &str = "64mb";
/// Dataset + write-amplification headroom + the S21 compaction reserve.
const TIER_DISK_BUDGET: &str = "4gb";

struct DataDirGuard(PathBuf);

impl DataDirGuard {
    fn create(path: PathBuf) -> Result<DataDirGuard, String> {
        std::fs::create_dir_all(&path).map_err(|e| format!("data dir {path:?}: {e}"))?;
        Ok(DataDirGuard(path))
    }
    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for DataDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The document leg: pipelined `JSON.SET`/`JSON.GET` (1:5) over the
/// seeded M3 corpus — the `load.rs` loop shape with document commands
/// (that generator is deliberately SET/GET-only; documents build their
/// wire bytes from `doc_corpus::instance`, the corpus of record).
#[derive(Clone)]
struct DocSpec {
    port: u16,
    conns: usize,
    pipeline: usize,
    duration: Duration,
    warmup: Duration,
    seed: u64,
}

fn doc_key(index: u64) -> Vec<u8> {
    format!("doc:{index:06}").into_bytes()
}

fn run_doc_conn(
    spec: &DocSpec,
    conn_index: usize,
    warmup_end: Instant,
    deadline: Instant,
) -> Result<(u64, u64, LogHistogram), String> {
    let mut stream = connect("127.0.0.1", spec.port)?;
    let reply = request(&mut stream, &[b"INF.NS", b"USE", DOC_NS.as_bytes()])?;
    if reply.starts_with(b"-") {
        return Err(format!("doc setup USE failed: {}", String::from_utf8_lossy(&reply)));
    }
    let mut rng = inf_foundation::rng::SplitMix64::new(spec.seed ^ (0xD0C5 + conn_index as u64));
    use inf_foundation::rng::Entropy;
    let mut hist = LogHistogram::new();
    let (mut ops, mut errors) = (0u64, 0u64);
    let mut inflight: std::collections::VecDeque<Instant> =
        std::collections::VecDeque::with_capacity(spec.pipeline);
    let mut rx: Vec<u8> = Vec::with_capacity(64 * 1024);
    let mut rx_at = 0usize;
    let mut tx: Vec<u8> = Vec::with_capacity(16 * 1024);
    let mut chunk = [0u8; 64 * 1024];
    let mut done_sending = false;
    loop {
        tx.clear();
        while inflight.len() < spec.pipeline && !done_sending {
            if Instant::now() >= deadline {
                done_sending = true;
                break;
            }
            let index = rng.next_u64() % DOC_KEYS;
            let key = doc_key(index);
            if rng.next_u64().is_multiple_of(6) {
                let doc = crate::doc_corpus::instance(spec.seed, DOC_SHAPE, index);
                tx.extend_from_slice(&encode_command(&[b"JSON.SET", &key, b"$", doc.as_bytes()]));
            } else {
                tx.extend_from_slice(&encode_command(&[b"JSON.GET", &key, b"$"]));
            }
            inflight.push_back(Instant::now());
        }
        if !tx.is_empty() {
            use std::io::Write as _;
            stream.write_all(&tx).map_err(|e| format!("doc write: {e}"))?;
        }
        if inflight.is_empty() {
            break;
        }
        let n = stream.read(&mut chunk).map_err(|e| format!("doc read: {e}"))?;
        if n == 0 {
            return Err("server closed the document connection under load".into());
        }
        rx.extend_from_slice(&chunk[..n]);
        while let Some(end) = reply_len(&rx[rx_at..]) {
            let sent = inflight.pop_front().ok_or("doc reply without a request")?;
            if sent >= warmup_end {
                hist.record(sent.elapsed().as_micros() as u64);
                ops += 1;
            }
            if rx[rx_at] == b'-' {
                errors += 1;
            }
            rx_at += end;
            if inflight.is_empty() {
                break;
            }
        }
        if rx_at > 0 {
            rx.drain(..rx_at);
            rx_at = 0;
        }
    }
    Ok((ops, errors, hist))
}

fn run_doc(spec: &DocSpec) -> Result<LoadReport, String> {
    let started = Instant::now();
    let warmup_end = started + spec.warmup;
    let deadline = warmup_end + spec.duration;
    let results: Vec<Result<(u64, u64, LogHistogram), String>> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..spec.conns)
            .map(|i| scope.spawn(move || run_doc_conn(spec, i, warmup_end, deadline)))
            .collect();
        handles.into_iter().map(|h| h.join().expect("doc conn thread")).collect()
    });
    let elapsed = started.elapsed().saturating_sub(spec.warmup);
    let mut report = LoadReport { elapsed_s: elapsed.as_secs_f64(), ..Default::default() };
    let mut hist = LogHistogram::new();
    for result in results {
        let (ops, errors, h) = result?;
        report.ops += ops;
        report.errors += errors;
        hist.merge(&h);
    }
    report.ops_per_sec = report.ops as f64 / report.elapsed_s;
    report.p50_us = hist.percentile(50.0);
    report.p99_us = hist.percentile(99.0);
    report.p999_us = hist.percentile(99.9);
    report.p9999_us = hist.percentile(99.99);
    report.max_us = hist.max();
    Ok(report)
}

fn cache_spec(port: u16, duration: Duration, conns: usize, seed: u64) -> LoadSpec {
    LoadSpec {
        port,
        conns,
        pipeline: CACHE_PIPELINE,
        duration,
        set_weight: 1,
        get_weight: 1,
        keys: CACHE_KEYS,
        key_prefix: "c:".into(),
        value_size: CACHE_VALUE,
        seed,
        ..Default::default()
    }
}

/// The tiered leg: uniform 1:1 SET/GET on `audit-tier`, deeply
/// pipelined. `setup` carries the `INF.NS USE` that S26 made legal.
fn tier_spec(port: u16, duration: Duration, seed: u64) -> LoadSpec {
    LoadSpec {
        port,
        conns: TIER_CONNS,
        pipeline: TIER_PIPELINE,
        duration,
        set_weight: 1,
        get_weight: 1,
        keys: TIER_KEYS,
        key_prefix: "t:".into(),
        value_size: TIER_VALUE,
        seed,
        setup: vec![vec![b"INF.NS".to_vec(), b"USE".to_vec(), TIER_NS.as_bytes().to_vec()]],
        ..Default::default()
    }
}

fn control(port: u16, argv: &[&[u8]]) -> Result<Vec<u8>, String> {
    // A durable node's socket accepts before recovery finishes; the
    // typed `-LOADING` refusal is the ready probe (bounded wait).
    for _ in 0..100 {
        let mut stream = connect("127.0.0.1", port)?;
        let reply = request(&mut stream, argv)?;
        if reply.starts_with(b"-LOADING") {
            #[allow(clippy::disallowed_methods)] // bench control path, not cell code
            std::thread::sleep(Duration::from_millis(100));
            continue;
        }
        if reply.starts_with(b"-") {
            return Err(format!(
                "control command {:?} failed: {}",
                String::from_utf8_lossy(argv[0]),
                String::from_utf8_lossy(&reply)
            ));
        }
        return Ok(reply);
    }
    Err("server stayed in LOADING for 10 s".into())
}

/// The 12 M2 attribution domains plus the RSS-resident tiering terms
/// (committed ring pages + index/sidecar). `tiering_reserved_bytes` is
/// deliberately absent: reserved VA is not resident memory, and adding
/// it would "improve" divergence with bytes RSS never saw.
fn sum_domains(infos: &[std::collections::BTreeMap<String, String>]) -> u64 {
    sum_field(infos, "records_resident_bytes")
        + sum_field(infos, "index_bytes")
        + sum_field(infos, "wheel_bytes")
        + sum_field(infos, "evict_bytes")
        + sum_field(infos, "doc_resident_bytes")
        + sum_field(infos, "doc_scratch_bytes")
        + sum_field(infos, "doc_path_cache_bytes")
        + sum_field(infos, "wire_buffers_bytes")
        + sum_field(infos, "conn_state_bytes")
        + sum_field(infos, "pubsub_state_bytes")
        + sum_field(infos, "log_staging_bytes")
        + sum_field(infos, "ckpt_buffer_bytes")
        + sum_field(infos, "tiering_committed_bytes")
        + sum_field(infos, "tiering_index_bytes")
}

fn cpus_allowed() -> String {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("Cpus_allowed_list:"))
                .map(|l| l.split_whitespace().nth(1).unwrap_or("?").to_string())
        })
        .unwrap_or_else(|| "?".into())
}

fn server_version(bin: &str) -> String {
    std::process::Command::new(bin).arg("--version").output().map_or_else(
        |e| format!("unreadable ({e})"),
        |o| String::from_utf8_lossy(&o.stdout).trim().to_string(),
    )
}

fn leg_line(name: &str, r: &LoadReport) -> String {
    format!(
        "| {name} | {:.0} | {} | {} | {} | {} | {} |",
        r.ops_per_sec, r.p50_us, r.p99_us, r.p999_us, r.errors, r.nils
    )
}

/// `inf-bench mixed-audit` — see the module doc. Artifacts under
/// `--artifacts-root` (default `.artifacts/m4/s20`).
pub fn cmd_mixed_audit(args: &[String]) -> Result<(), String> {
    let flags = Flags::parse(
        args,
        &["unsafe-env", "allow-dirty", "reference-box"],
        &[
            "duration",
            "cells",
            "infinityd-bin",
            "pin-start",
            "artifacts-root",
            "seed",
            "data-root",
            // `known` includes the bool flags (the Flags contract).
            "unsafe-env",
            "allow-dirty",
            "reference-box",
        ],
    )?;
    let duration = Duration::from_secs(
        flags
            .get("duration")
            .map_or(Ok(20), |v| v.parse())
            .map_err(|e| format!("--duration: {e}"))?,
    );
    let cells: u16 =
        flags.get("cells").map_or(Ok(4), |v| v.parse()).map_err(|e| format!("--cells: {e}"))?;
    let seed: u64 = flags
        .get("seed")
        .map_or(Ok(0x51D0_2026), |v| v.parse())
        .map_err(|e| format!("--seed: {e}"))?;
    let bin = flags.str_or("infinityd-bin", "target/release/infinityd");
    let pin_start = flags.str_or("pin-start", "4");
    let artifacts_root = flags.str_or("artifacts-root", ".artifacts/m4/s20");
    let data_root = flags.str_or("data-root", &std::env::temp_dir().to_string_lossy());
    let reference_box = flags.bool("reference-box");
    let env_ok = crate::gaterun::env_gate(&flags)?;

    let guard = DataDirGuard::create(
        PathBuf::from(&data_root).join(format!("inf-m4-s20-{}", std::process::id())),
    )?;
    let dir_s = guard.path().to_string_lossy().into_owned();
    let server: ServerGuard =
        spawn_infinityd(&bin, cells, &["--data-dir", &dir_s, "--pin-start", &pin_start])?;
    let port = server.port;

    // The three namespaces of the §4.3 profile: the cache is the
    // default DB under node-level maxmemory + allkeys-lru (see the
    // NODE_MAXMEMORY finding), documents get a named memory namespace,
    // and the tiered namespace stands with real budgets.
    control(port, &[b"CONFIG", b"SET", b"maxmemory", NODE_MAXMEMORY.as_bytes()])?;
    control(port, &[b"CONFIG", b"SET", b"maxmemory-policy", b"allkeys-lru"])?;
    control(port, &[b"INF.NS", b"CREATE", DOC_NS.as_bytes(), b"MODE", b"memory"])?;
    control(
        port,
        &[
            b"INF.NS",
            b"CREATE",
            TIER_NS.as_bytes(),
            b"MODE",
            b"durable",
            b"MEM-BUDGET",
            TIER_MEM_BUDGET.as_bytes(),
            b"DISK-BUDGET",
            TIER_DISK_BUDGET.as_bytes(),
        ],
    )?;
    // The inverted probe (M4-S26 lifted the D8 refusal): the tiered data
    // plane must be *reachable*. The old assertion checked for the
    // refusal, which is how a named-absent row stays alive past the work
    // that should have retired it — the F17/F18 shape. This one fails if
    // the third workload cannot run, so it can never quietly not run.
    let mut probe = connect("127.0.0.1", port)?;
    let reply = request(&mut probe, &[b"INF.NS", b"USE", TIER_NS.as_bytes()])?;
    if reply.starts_with(b"-") {
        return Err(format!(
            "the tiered data plane is not reachable ({}) — M4-S26 wired it, so this is a \
             regression, not a reason to publish a two-workload audit as a three-workload one",
            String::from_utf8_lossy(&reply).trim()
        ));
    }
    drop(probe);

    // The attribution baseline (the M3 CI delta discipline): executable
    // text, thread stacks, and pre-sized structures are real RSS no
    // domain claims — capture both sides post-boot, pre-load, and let
    // the divergence rule bind on the *growth* the workloads cause.
    #[allow(clippy::disallowed_methods)] // bench settle, not cell code
    std::thread::sleep(Duration::from_secs(1));
    let rss_baseline = server.rss_bytes();
    let domains_baseline = sum_domains(&scrape_cells(port, cells)?);

    // The tiered dataset has to exist before anything can read it cold.
    // A fill converges rather than benchmarks: the durable admission path
    // answers typed refusals under sustained pipelined SETs, so the fill
    // is checked on DBSIZE, not on its own error count (the S22 lesson).
    println!(
        "== mixed-audit: tiered fill ({TIER_KEYS} keys x {TIER_VALUE} B = 10x {TIER_MEM_BUDGET}) =="
    );
    let tier_fill = run_load(&LoadSpec {
        fill: Some(TIER_KEYS),
        pipeline: 64,
        conns: 8,
        ..tier_spec(port, duration, seed ^ 0x7F)
    })?;
    println!("{}", render(&tier_fill));

    // Solo baselines (same campaign, same box, same config — §19).
    println!("== mixed-audit: cache solo ==");
    let cache_solo = run_load(&cache_spec(port, duration, CACHE_CONNS, seed))?;
    println!("{}", render(&cache_solo));
    println!("== mixed-audit: document solo ==");
    let doc_solo = run_doc(&DocSpec {
        port,
        conns: DOC_CONNS,
        pipeline: 4,
        duration,
        warmup: Duration::from_secs(1),
        seed,
    })?;
    println!("{}", render(&doc_solo));
    println!("== mixed-audit: tiered solo ==");
    let tier_pre_solo = scrape_cells(port, cells)?;
    let tier_solo = run_load(&tier_spec(port, duration, seed))?;
    let tier_post_solo = scrape_cells(port, cells)?;
    println!("{}", render(&tier_solo));

    // §19 saturation probe: the cache leg again at +50% connections for
    // half the duration — if throughput moves materially, the generator
    // (not the server) set the solo number.
    println!("== mixed-audit: saturation probe (cache, +50% conns) ==");
    let probe_report =
        run_load(&cache_spec(port, duration / 2, CACHE_CONNS + CACHE_CONNS / 2, seed ^ 0x5A7))?;
    let probe_delta = delta_pct(cache_solo.ops_per_sec, probe_report.ops_per_sec);
    let saturation = if probe_delta.abs() < 5.0 {
        format!(
            "generator unsaturated at {CACHE_CONNS} conns (+50% conns moved ops/s {probe_delta:+.1}% — \
             the solo number is server-set)"
        )
    } else {
        format!(
            "GENERATOR-LIMITED at {CACHE_CONNS} conns (+50% conns moved ops/s {probe_delta:+.1}% — solo \
             absolutes understate the server; deltas remain valid at fixed generator config)"
        )
    };
    println!("{saturation}");

    // The mixed run: cache + document concurrently, sampler watching
    // RSS (100 ms) and the attribution domains (~1 s, all cells).
    println!("== mixed-audit: mixed run (cache + document, sampler on) ==");
    let stop = AtomicBool::new(false);
    let rss_peak = AtomicU64::new(0);
    let worst_div_milli = AtomicU64::new(0);
    let div_samples = AtomicU64::new(0);
    let pid = server.pid();
    let tier_pre_mixed = scrape_cells(port, cells)?;
    let (cache_mixed, doc_mixed, tier_mixed) = std::thread::scope(|scope| {
        let sampler = scope.spawn(|| {
            while !stop.load(Ordering::Relaxed) {
                for _ in 0..10 {
                    #[allow(clippy::disallowed_methods)] // bench sampler, not cell code
                    std::thread::sleep(Duration::from_millis(100));
                    rss_peak.fetch_max(crate::gaterun::rss_bytes_of(pid), Ordering::Relaxed);
                    if stop.load(Ordering::Relaxed) {
                        return;
                    }
                }
                let rss_before = crate::gaterun::rss_bytes_of(pid);
                let Ok(infos) = scrape_cells(port, cells) else { continue };
                let rss_after = crate::gaterun::rss_bytes_of(pid);
                if rss_before == 0 || rss_after == 0 {
                    continue;
                }
                // Bracket the scrape with RSS reads: the domains are not
                // an instant, so pair them with the midpoint.
                let rss_now = (rss_before + rss_after) / 2;
                let domains = sum_domains(&infos);
                let rss_grow = rss_now.saturating_sub(rss_baseline);
                let dom_grow = domains.saturating_sub(domains_baseline);
                // Below the floor the ratio is noise, not attribution.
                if rss_grow < (32 << 20) {
                    continue;
                }
                let div = ((rss_grow as f64 - dom_grow as f64) / rss_grow as f64 * 100.0).abs();
                worst_div_milli.fetch_max((div * 1000.0) as u64, Ordering::Relaxed);
                div_samples.fetch_add(1, Ordering::Relaxed);
            }
        });
        let cache_handle =
            scope.spawn(|| run_load(&cache_spec(port, duration, CACHE_CONNS, seed ^ 0x11)));
        let doc_handle = scope.spawn(|| {
            run_doc(&DocSpec {
                port,
                conns: DOC_CONNS,
                pipeline: 4,
                duration,
                warmup: Duration::from_secs(1),
                seed: seed ^ 0x22,
            })
        });
        let tier_handle = scope.spawn(|| run_load(&tier_spec(port, duration, seed ^ 0x33)));
        let cache_mixed = cache_handle.join().expect("cache leg thread");
        let doc_mixed = doc_handle.join().expect("doc leg thread");
        let tier_mixed = tier_handle.join().expect("tier leg thread");
        stop.store(true, Ordering::Relaxed);
        sampler.join().expect("sampler thread");
        (cache_mixed, doc_mixed, tier_mixed)
    });
    let cache_mixed = cache_mixed?;
    let doc_mixed = doc_mixed?;
    let tier_mixed = tier_mixed?;
    println!("{}", render(&cache_mixed));
    println!("{}", render(&doc_mixed));
    println!("{}", render(&tier_mixed));

    // Post-run scrape. The liveness assertions are inverted from the D8
    // era (ADR-0071 D4's lesson: a leg that produced no load must fail
    // the run, not decorate it): the tiered namespace must stand on every
    // cell **and** show data-plane work, and the mixed leg must have
    // actually served cold reads or its isolation number describes
    // nothing.
    let infos = scrape_cells(port, cells)?;
    let tier_tables = sum_field(&infos, "tiering_tables");
    let tier_committed = sum_field(&infos, "tiering_committed_bytes");
    let tier_reserved = sum_field(&infos, "tiering_reserved_bytes");
    let tier_allocs = sum_field(&infos, "tiering_tail_allocs");
    let cold_delta = |pre: &[std::collections::BTreeMap<String, String>],
                      post: &[std::collections::BTreeMap<String, String>]| {
        sum_field(post, "cold_reads_issued").saturating_sub(sum_field(pre, "cold_reads_issued"))
    };
    let cold_solo = cold_delta(&tier_pre_solo, &tier_post_solo);
    let cold_mixed = cold_delta(&tier_pre_mixed, &infos);
    let cold_qd_p99 = infos
        .iter()
        .filter_map(|c| c.get("cold_read_qd_p99"))
        .filter_map(|v| v.parse::<u64>().ok())
        .max()
        .unwrap_or(0);
    let cold_p99_us = infos
        .iter()
        .filter_map(|c| c.get("tiering_cold_p99_us"))
        .filter_map(|v| v.parse::<u64>().ok())
        .max()
        .unwrap_or(0);
    let final_rss = server.rss_bytes();
    drop(server);
    drop(guard);
    if tier_tables != u64::from(cells) {
        return Err(format!(
            "the tiered namespace should stand on every cell ({tier_tables} tables, {cells} cells)"
        ));
    }
    if tier_allocs == 0 {
        return Err(
            "the tiered namespace served no data-plane work — the third workload did not run, \
             so this is not a three-workload audit"
                .into(),
        );
    }
    if cold_mixed == 0 {
        return Err(format!(
            "the tiered leg issued zero cold reads during the mixed run ({cold_solo} solo) — \
             the isolation row is about coexisting with cold-read traffic, and there was none"
        ));
    }

    // Deltas + verdicts.
    let cache_p99_delta = delta_pct(cache_solo.p99_us as f64, cache_mixed.p99_us as f64);
    let cache_ops_delta = delta_pct(cache_solo.ops_per_sec, cache_mixed.ops_per_sec);
    let doc_p99_delta = delta_pct(doc_solo.p99_us as f64, doc_mixed.p99_us as f64);
    let doc_ops_delta = delta_pct(doc_solo.ops_per_sec, doc_mixed.ops_per_sec);
    let tier_p99_delta = delta_pct(tier_solo.p99_us as f64, tier_mixed.p99_us as f64);
    let tier_ops_delta = delta_pct(tier_solo.ops_per_sec, tier_mixed.ops_per_sec);
    let worst_div = worst_div_milli.load(Ordering::Relaxed) as f64 / 1000.0;
    let samples = div_samples.load(Ordering::Relaxed);
    let miss_rate = |r: &LoadReport| {
        if r.ops == 0 { 0.0 } else { r.nils as f64 / r.ops as f64 * 100.0 }
    };

    let stamp = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let dir = PathBuf::from(&artifacts_root).join(format!("{stamp}-mixed-audit"));
    std::fs::create_dir_all(&dir).map_err(|e| format!("artifacts dir {dir:?}: {e}"))?;
    let mut out = String::new();
    let mut push = |line: &str| {
        out.push_str(line);
        out.push('\n');
    };
    push("# M4-S20 mixed-node coexistence audit");
    push("");
    push(&format!(
        "- date: {stamp} (unix) · tier: **{}** · env-check: {}",
        if reference_box { "reference-box (binding)" } else { "dev (non-binding)" },
        if env_ok { "pass" } else { "FAILED (recorded, non-citable)" },
    ));
    push(&format!("- server: {} · {cells} cells · pin-start {pin_start}", server_version(&bin)));
    push(&format!(
        "- loadgen cpus_allowed: {} (server cells pinned from {pin_start}; a loadgen set \
         overlapping the cell cpus invalidates the run — the C5 lesson)",
        cpus_allowed()
    ));
    push(&format!(
        "- seed {seed:#x} · {}s per leg · cache = default DB, node maxmemory {NODE_MAXMEMORY} \
         allkeys-lru, {CACHE_KEYS} keys × {CACHE_VALUE} B offered · documents {DOC_KEYS} × \
         {DOC_SHAPE} on `{DOC_NS}` · tiered {TIER_KEYS} × {TIER_VALUE} B = 10× \
         {TIER_MEM_BUDGET} on `{TIER_NS}` (disk budget {TIER_DISK_BUDGET})",
        duration.as_secs()
    ));
    push(&format!(
        "- generator placement: cache {CACHE_CONNS} conns × pipeline {CACHE_PIPELINE}, \
         documents {DOC_CONNS} × 4, tiered {TIER_CONNS} × {TIER_PIPELINE}. The cache and \
         document legs run the identical connection counts solo and mixed; the tiered leg \
         takes its queue depth from pipelining rather than threads, so the mixed leg adds \
         {TIER_CONNS} mostly-I/O-blocked generator threads rather than a second workload's \
         worth of CPU (the C5 lesson — a mixed leg whose generator is crowded measures \
         generator crowding)"
    ));
    push("");
    push("## Legs");
    push("");
    push("| leg | ops/s | p50 µs | p99 µs | p99.9 µs | errors | nil replies |");
    push("|---|---|---|---|---|---|---|");
    push(&leg_line("cache solo", &cache_solo));
    push(&leg_line("document solo", &doc_solo));
    push(&leg_line("tiered solo", &tier_solo));
    push(&leg_line("cache mixed", &cache_mixed));
    push(&leg_line("document mixed", &doc_mixed));
    push(&leg_line("tiered mixed", &tier_mixed));
    push("");
    push(&format!(
        "Tiered fill: {} keys at {:.0} sets/s, {} error replies (typed durable admission \
         backpressure is a refusal, not a loss — the fill is checked on convergence).",
        TIER_KEYS, tier_fill.ops_per_sec, tier_fill.errors
    ));
    push("");
    push("## Isolation (solo → mixed, same campaign)");
    push("");
    push("| namespace | ops/s Δ | p99 Δ | miss-rate solo → mixed |");
    push("|---|---|---|---|");
    push(&format!(
        "| cache | {cache_ops_delta:+.1}% | {cache_p99_delta:+.1}% | {:.1}% → {:.1}% |",
        miss_rate(&cache_solo),
        miss_rate(&cache_mixed)
    ));
    push(&format!("| documents | {doc_ops_delta:+.1}% | {doc_p99_delta:+.1}% | — |"));
    push(&format!("| tiered | {tier_ops_delta:+.1}% | {tier_p99_delta:+.1}% | — |"));
    push("");
    push(&format!(
        "Cold-read evidence for the isolation condition (\"while the tiered ns serves cold \
         reads at full QD\"): **{cold_mixed} cold reads issued during the mixed run** \
         ({cold_solo} solo), cold-read queue depth p99 **{cold_qd_p99}** (cap 64, ADR-0055 \
         D2), tiered cold service p99 **{:.1} ms**, {tier_allocs} tail allocations, \
         {tier_committed} B committed. A run that reached the mixed leg without cold reads \
         fails rather than reports.",
        cold_p99_us as f64 / 1000.0
    ));
    push("");
    push(&format!(
        "Gate `cache_isolation_p99` (≤ 10%, reference-box): measured {cache_p99_delta:+.1}% — \
         {}{}",
        if cache_p99_delta.abs() <= 10.0 { "PASS" } else { "FAIL" },
        if reference_box { "" } else { " (DEV-TIER, non-binding)" },
    ));
    push("");
    push("## Attribution (continuous, mixed run)");
    push("");
    push(&format!(
        "- sum(domains) vs RSS worst divergence: **{worst_div:.1}%** over {samples} samples — \
         computed on growth over the post-boot baseline (the M3 CI delta discipline: \
         executable text/stacks are RSS no domain claims), RSS bracketing the ~1 s domain \
         scrape, 32 MiB growth floor; 12 M2 domains + tiering committed/index, reserved VA \
         excluded (not resident)",
    ));
    push(&format!(
        "- baselines: RSS {rss_baseline} B, domains {domains_baseline} B (post-boot, pre-load)"
    ));
    push(&format!(
        "- Gate `mixed_attribution` (≤ 10%, any tier): {}",
        if worst_div <= 10.0 { "PASS" } else { "FAIL" }
    ));
    push(&format!(
        "- peak RSS {} B · final RSS {} B · standing tiered reservation {tier_reserved} B VA, \
         {tier_committed} B committed",
        rss_peak.load(Ordering::Relaxed),
        final_rss
    ));
    push(&format!(
        "- page-cache disclosure: the tiered leg does real file I/O this run \
          ({tier_committed} B committed, {cold_mixed} cold reads in the mixed leg). S09 chose \
          `Direct` (ADR-0054), so tier reads bypass the page cache and no file-cache term is \
          claimed against RSS; `tiering_reserved_bytes` ({tier_reserved} B) is VA, not \
          resident, and is excluded from the domain sum for that reason"
    ));
    push("");
    push("## Saturation disposition (§19)");
    push("");
    push(&format!("- cache generator: {saturation}"));
    push(
        "- document generator: not probed this run — the doc leg's absolutes are context, \
          not claims; its isolation *delta* is the audit quantity (fixed generator config \
          both sides)",
    );
    push(
        "- tiered generator: not probed. This leg is device-bound by construction (uniform \
          keys over 10× its memory budget on a Gen3 DRAM-less NVMe), so a connection probe \
          would measure the drive, not the generator; its absolutes are context and its \
          isolation delta is the audit quantity",
    );
    push("");
    push("## Findings");
    push("");
    push(
        "- **Per-namespace `MAXMEMORY` on named memory namespaces is unenforced**: the \
          registry carries it (M1) but the eviction sweep rotates the numbered dbs only \
          (`Keyspace::evict_toward`), so a named cache namespace never evicts. This audit \
          therefore runs the cache leg on the default DB under node-level `CONFIG SET \
          maxmemory` (the proven M1 machinery). Recorded for the plan: per-namespace \
          eviction enforcement needs an owner before a multi-cache node is honest.",
    );
    push("");
    push("## Named absent (debt-forward, honesty rules)");
    push("");
    push("| row | why absent | rejoins |");
    push("|---|---|---|");
    push(
        "| *(the tiered data leg is no longer absent — it ran this campaign; the row is kept \
         here struck through so the audit's own history stays readable)* | — | — |",
    );
    push("| topic workload | M7 owns topics | M7, per the plan's debt-forward note |");
    push("| collections workload | M5 owns collections | M5, per the plan's debt-forward note |");
    let report_path = dir.join("report.md");
    std::fs::write(&report_path, &out).map_err(|e| format!("write {report_path:?}: {e}"))?;
    println!("\nmixed-audit report: {}", report_path.display());
    print!("{out}");
    Ok(())
}
