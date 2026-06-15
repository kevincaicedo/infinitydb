//! `inf-bench zipfian` (M1-S17, hit-rate-parity gate `hit_rate_parity`):
//! the zipfian LFU trace-replay campaign tool.
//!
//! Drives an identical zipfian (θ≈0.99) cache-aside trace against InfinityDB
//! and real Redis, both under `allkeys-lfu` at the same `maxmemory`, and
//! reports each engine's hit rate and the gap (`pp below Redis`). The gate
//! passes when InfinityDB is within `--threshold-pp` (default 2.0) of Redis.
//!
//! Unlike the latency/throughput rows, **hit rate is an algorithm property,
//! not a speed measurement** — it does not depend on CPU governor or thermal
//! state, so a clean dev-box run is indicative (the only machine sensitivity
//! is the LFU time-decay, which both engines share). Per L10 the binding
//! milestone verdict still wants `--reference-box`, but the number reproduces
//! anywhere the same trace is replayed.
//!
//! Tooling tier: blocking sockets + `std::thread` are fine here; this binary
//! never runs on the data plane.

use std::io::{Read as _, Write as _};
use std::net::TcpStream;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::cli::Flags;
use crate::gaterun::{ServerGuard, spawn_infinityd, spawn_redis};
use crate::resp::{connect, encode_command, reply_len};

/// Deterministic SplitMix64 — the trace must be byte-for-byte identical
/// across both engines, so the only randomness is this seeded stream.
struct SplitMix64(u64);

impl SplitMix64 {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in [0, 1) with 53 bits of entropy.
    fn next_f64(&mut self) -> f64 {
        ((self.next_u64() >> 11) as f64) * (1.0 / 9_007_199_254_740_992.0)
    }
}

/// Parameters for one parity run (shared by the subcommand and the optional
/// `gate-run m1` row).
pub struct ParityParams {
    pub keyspace: u32,
    pub warmup: u64,
    pub ops: u64,
    pub value_size: usize,
    pub theta: f64,
    pub seed: u64,
    pub maxmemory_bytes: u64,
    pub cells: u16,
    pub window: usize,
}

pub struct EngineResult {
    pub hits: u64,
    pub total: u64,
}

impl EngineResult {
    pub fn hit_rate(&self) -> f64 {
        if self.total == 0 { 0.0 } else { self.hits as f64 / self.total as f64 }
    }
}

pub struct ParityResult {
    pub infinity: EngineResult,
    pub redis: EngineResult,
}

impl ParityResult {
    /// Percentage points by which InfinityDB trails Redis (negative ⇒ ahead).
    pub fn pp_below(&self) -> f64 {
        (self.redis.hit_rate() - self.infinity.hit_rate()) * 100.0
    }
}

/// Cumulative zipfian weights over ranks `1..=keyspace`: `w_i = 1 / i^θ`.
/// `partition_point` over this turns a uniform draw into a rank.
fn build_cdf(keyspace: u32, theta: f64) -> Vec<f64> {
    let mut cdf = Vec::with_capacity(keyspace as usize);
    let mut acc = 0.0f64;
    for rank in 1..=u64::from(keyspace) {
        acc += 1.0 / (rank as f64).powf(theta);
        cdf.push(acc);
    }
    cdf
}

fn sample(cdf: &[f64], total_weight: f64, rng: &mut SplitMix64) -> u32 {
    let u = rng.next_f64() * total_weight;
    // Smallest index whose cumulative weight is ≥ u.
    let idx = cdf.partition_point(|&c| c < u);
    idx.min(cdf.len() - 1) as u32
}

/// Frames pipelined RESP replies out of a streaming socket, classifying each
/// as nil (a cache miss) or not, without allocating per reply.
struct ReplyReader {
    stream: TcpStream,
    buf: Vec<u8>,
}

impl ReplyReader {
    fn new(stream: TcpStream) -> ReplyReader {
        ReplyReader { stream, buf: Vec::with_capacity(64 * 1024) }
    }

    fn fill(&mut self) -> Result<(), String> {
        let mut chunk = [0u8; 32 * 1024];
        let n = self.stream.read(&mut chunk).map_err(|e| format!("read: {e}"))?;
        if n == 0 {
            return Err("server closed the connection mid-replay".into());
        }
        self.buf.extend_from_slice(&chunk[..n]);
        Ok(())
    }

    /// Consumes exactly one reply; returns whether it was a RESP nil.
    fn next_is_nil(&mut self) -> Result<bool, String> {
        loop {
            if let Some(n) = reply_len(&self.buf) {
                let nil = &self.buf[..n] == b"$-1\r\n" || &self.buf[..n] == b"_\r\n";
                self.buf.drain(..n);
                return Ok(nil);
            }
            self.fill()?;
        }
    }

    /// Consumes `count` replies, ignoring their contents (SET acks).
    fn consume(&mut self, count: usize) -> Result<(), String> {
        for _ in 0..count {
            self.next_is_nil()?;
        }
        Ok(())
    }
}

fn control(port: u16, argv: &[&[u8]]) -> Result<(), String> {
    let mut conn = connect("127.0.0.1", port)?;
    conn.write_all(&encode_command(argv)).map_err(|e| format!("write: {e}"))?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        if let Some(n) = reply_len(&buf) {
            if buf[..n].starts_with(b"-") {
                return Err(format!("{:?} -> {}", argv[0], String::from_utf8_lossy(&buf[..n])));
            }
            return Ok(());
        }
        let n = conn.read(&mut chunk).map_err(|e| format!("read: {e}"))?;
        if n == 0 {
            return Err("control connection closed".into());
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

fn configure_lfu(port: u16, maxmemory_bytes: u64) -> Result<(), String> {
    control(port, &[b"CONFIG", b"SET", b"maxmemory", maxmemory_bytes.to_string().as_bytes()])?;
    control(port, &[b"CONFIG", b"SET", b"maxmemory-policy", b"allkeys-lfu"])
}

/// Replays the seeded cache-aside trace against one server. The trace is
/// regenerated from `seed` (not stored), so both engines see an identical
/// access sequence. Hits are counted only over the measured tail (`ops`),
/// after `warmup` accesses fill the cache to steady state.
fn replay(
    port: u16,
    cdf: &[f64],
    total_weight: f64,
    p: &ParityParams,
) -> Result<EngineResult, String> {
    let value = vec![b'x'; p.value_size];
    let mut rng = SplitMix64(p.seed);
    let mut reader = ReplyReader::new(connect("127.0.0.1", port)?);

    let mut hits = 0u64;
    let mut total = 0u64;
    let grand_total = p.warmup + p.ops;
    let mut done = 0u64;

    while done < grand_total {
        let batch = p.window.min((grand_total - done) as usize);
        // Pipeline a window of GETs.
        let mut keys: Vec<Vec<u8>> = Vec::with_capacity(batch);
        let mut out = Vec::with_capacity(batch * 32);
        for _ in 0..batch {
            let idx = sample(cdf, total_weight, &mut rng);
            let key = format!("z:{idx}").into_bytes();
            out.extend_from_slice(&encode_command(&[b"GET", &key]));
            keys.push(key);
        }
        reader.stream.write_all(&out).map_err(|e| format!("write gets: {e}"))?;

        // Read the window's replies; misses become SETs (cache-aside fill).
        let mut misses: Vec<&[u8]> = Vec::new();
        for (j, key) in keys.iter().enumerate() {
            let measured = done + j as u64 >= p.warmup;
            let miss = reader.next_is_nil()?;
            if measured {
                total += 1;
                if !miss {
                    hits += 1;
                }
            }
            if miss {
                misses.push(key);
            }
        }
        if !misses.is_empty() {
            let mut out = Vec::with_capacity(misses.len() * (32 + p.value_size));
            for key in &misses {
                out.extend_from_slice(&encode_command(&[b"SET", key, &value]));
            }
            reader.stream.write_all(&out).map_err(|e| format!("write sets: {e}"))?;
            reader.consume(misses.len())?;
        }
        done += batch as u64;
    }

    Ok(EngineResult { hits, total })
}

/// Spawns both engines under `allkeys-lfu`, replays the identical trace, and
/// returns each engine's hit rate.
pub fn run_parity(
    infinityd_bin: &str,
    redis_bin: &str,
    p: &ParityParams,
) -> Result<ParityResult, String> {
    let cdf = build_cdf(p.keyspace, p.theta);
    let total_weight = *cdf.last().ok_or("keyspace must be ≥ 1")?;

    // InfinityDB leg (spawn_infinityd already passes `--cells`).
    let ours: ServerGuard = spawn_infinityd(infinityd_bin, p.cells, &[])?;
    configure_lfu(ours.port, p.maxmemory_bytes)?;
    let infinity = replay(ours.port, &cdf, total_weight, p)?;
    drop(ours);

    // Redis leg (same trace, same budget, same policy).
    let redis = spawn_redis(redis_bin)?;
    configure_lfu(redis.port, p.maxmemory_bytes)?;
    let redis_res = replay(redis.port, &cdf, total_weight, p)?;
    drop(redis);

    Ok(ParityResult { infinity, redis: redis_res })
}

fn unix_stamp() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

const KNOWN: &[&str] = &[
    "keyspace",
    "ops",
    "warmup",
    "value-size",
    "theta",
    "seed",
    "maxmemory-mb",
    "cells",
    "window",
    "threshold-pp",
    "infinityd-bin",
    "redis-bin",
    "artifacts-root",
    "reference-box",
];

pub fn cmd_zipfian(args: &[String]) -> Result<(), String> {
    let flags = Flags::parse(args, &["reference-box"], KNOWN)?;
    let keyspace = flags.u64_or("keyspace", 200_000)?;
    let ops = flags.u64_or("ops", 1_000_000)?;
    let warmup = flags.u64_or("warmup", ops)?;
    let value_size = flags.usize_or("value-size", 64)?;
    let theta = flags.f64_or("theta", 0.99)?;
    let seed = flags.u64_or("seed", 0x5EED_2026_C0DE)?;
    let maxmemory_mb = flags.u64_or("maxmemory-mb", 8)?;
    let cells = flags.u16_or("cells", 1)?;
    let window = flags.usize_or("window", 256)?.max(1);
    let threshold_pp = flags.f64_or("threshold-pp", 2.0)?;
    let infinityd_bin = flags.str_or("infinityd-bin", "target/release/infinityd");
    let redis_bin = flags.str_or("redis-bin", "redis-server");
    let artifacts_root = flags.str_or("artifacts-root", ".artifacts/m1");
    let reference_box = flags.bool("reference-box");

    if keyspace == 0 || keyspace > u64::from(u32::MAX) {
        return Err("--keyspace must be in 1..=4294967295".into());
    }
    if !(0.0..=5.0).contains(&theta) {
        return Err("--theta must be in 0.0..=5.0".into());
    }

    let params = ParityParams {
        keyspace: keyspace as u32,
        warmup,
        ops,
        value_size,
        theta,
        seed,
        maxmemory_bytes: maxmemory_mb * 1024 * 1024,
        cells,
        window,
    };

    let tier = if reference_box { "reference-box" } else { "DEV-TIER (indicative)" };
    println!(
        "zipfian LFU parity [{tier}]: keyspace {keyspace}, θ={theta}, maxmemory {maxmemory_mb} MiB, \
         {warmup} warmup + {ops} measured ops, {cells} cell(s)"
    );

    let result = run_parity(&infinityd_bin, &redis_bin, &params)?;
    let infinity_pct = result.infinity.hit_rate() * 100.0;
    let redis_pct = result.redis.hit_rate() * 100.0;
    let pp = result.pp_below();
    let pass = pp <= threshold_pp;

    println!("  InfinityDB allkeys-lfu hit rate: {infinity_pct:.2}%");
    println!("  Redis 8 allkeys-lfu hit rate:    {redis_pct:.2}%");
    println!("  gap (pp below Redis): {pp:+.2} pp  (threshold ≤ {threshold_pp:.2})");
    println!("  verdict: {}", if pass { "PASS" } else { "FAIL" });

    // Artifact: the hit_rate_parity gate's external evidence.
    let stamp = unix_stamp();
    let dir = format!("{artifacts_root}/{stamp}-zipfian");
    std::fs::create_dir_all(&dir).map_err(|e| format!("create {dir}: {e}"))?;
    let report = format!(
        "# zipfian LFU hit-rate parity\n\n\
         tier: {tier}\n\
         seed: {seed:#x} · keyspace: {keyspace} · theta: {theta} · maxmemory: {maxmemory_mb} MiB · \
         cells: {cells} · value: {value_size} B · ops: {ops} (warmup {warmup})\n\n\
         | engine | policy | hits | measured | hit rate |\n\
         |---|---|---|---|---|\n\
         | InfinityDB | allkeys-lfu | {ih} | {it} | {infinity_pct:.3}% |\n\
         | Redis 8 | allkeys-lfu | {rh} | {rt} | {redis_pct:.3}% |\n\n\
         gap (pp below Redis): {pp:+.3} · threshold: ≤ {threshold_pp:.2} pp · verdict: {verdict}\n\n\
         gate row: `hit_rate_parity` (source `external:zipfian_lfu`). \
         Hit rate is an eviction-algorithm property and reproduces independent of \
         CPU governor/thermal state; the only machine sensitivity is the shared LFU \
         time-decay. Binding milestone verdicts still require `--reference-box` (L10).\n",
        ih = result.infinity.hits,
        it = result.infinity.total,
        rh = result.redis.hits,
        rt = result.redis.total,
        verdict = if pass { "PASS" } else { "FAIL" },
    );
    let path = format!("{dir}/report.md");
    std::fs::write(&path, report).map_err(|e| format!("write {path}: {e}"))?;
    println!("  report written to {path}");

    if pass {
        Ok(())
    } else {
        Err(format!(
            "hit-rate parity FAILED: {pp:+.2} pp below Redis (threshold {threshold_pp:.2})"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splitmix_is_deterministic() {
        // The parity comparison is only fair if both engines replay an
        // identical trace — that rests entirely on this seeded stream.
        let mut a = SplitMix64(0xDEAD_BEEF);
        let mut b = SplitMix64(0xDEAD_BEEF);
        for _ in 0..1000 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
        let mut c = SplitMix64(0xDEAD_BEEF);
        let mut f = SplitMix64(0xDEAD_BEEF);
        for _ in 0..1000 {
            let x = c.next_f64();
            assert!((0.0..1.0).contains(&x));
            assert_eq!(x.to_bits(), f.next_f64().to_bits());
        }
    }

    #[test]
    fn zipf_sampling_is_skewed_and_deterministic() {
        let keyspace = 10_000u32;
        let cdf = build_cdf(keyspace, 0.99);
        let total = *cdf.last().unwrap();
        assert_eq!(cdf.len(), keyspace as usize);

        // Same seed ⇒ identical sample sequence.
        let mut r1 = SplitMix64(7);
        let mut r2 = SplitMix64(7);
        let mut hot = 0u64; // draws landing in the top 1% of ranks
        let n = 100_000;
        for _ in 0..n {
            let s1 = sample(&cdf, total, &mut r1);
            assert_eq!(s1, sample(&cdf, total, &mut r2));
            assert!(s1 < keyspace);
            if s1 < keyspace / 100 {
                hot += 1;
            }
        }
        // θ≈0.99 is heavily skewed: the hottest 1% of ranks should take a
        // large share of accesses (far above the uniform 1%).
        assert!(hot * 100 > n * 30, "expected >30% of draws in the top 1%, got {hot}/{n}");
    }
}
