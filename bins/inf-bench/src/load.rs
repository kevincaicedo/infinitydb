//! Native pipelined RESP load generator (M0-S18): N blocking connections on
//! N threads, fixed pipeline depth, seeded SET/GET mix, per-command latency
//! into merged `LogHistogram`s. Also the deterministic fill mode (each
//! connection SETs a partitioned key range exactly once) for the RSS gate.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::time::{Duration, Instant};

use inf_foundation::LogHistogram;
use inf_foundation::rng::{Entropy, SplitMix64};

use crate::cli::Flags;
use crate::resp::{connect, encode_command, reply_len};

#[derive(Clone, Debug)]
pub struct LoadSpec {
    pub host: String,
    pub port: u16,
    pub conns: usize,
    pub pipeline: usize,
    pub duration: Duration,
    /// SET weight out of `set_weight + get_weight` (mix "1:10" ⇒ 1, 10).
    pub set_weight: u64,
    pub get_weight: u64,
    pub keys: u64,
    pub key_prefix: String,
    pub key_size: usize,
    pub value_size: usize,
    pub seed: u64,
    /// Stats reset after this ramp (cold connects + first batches excluded).
    pub warmup: Duration,
    /// Fill mode: SET exactly this many keys (partitioned), ignore duration.
    pub fill: Option<u64>,
    /// M1 TTL-heavy rows: every SET carries `PX <seeded uniform in range>`.
    pub ttl_range_ms: Option<(u64, u64)>,
    /// M1 expiry-storm fill: every SET carries `PXAT <abs unix ms>` — the
    /// whole fill expires at one instant (the 1M-same-second storm shape).
    pub pxat_ms: Option<u64>,
    /// Commands each connection sends (and awaits, error-checked) before
    /// the load starts — connection state like `INF.NS USE` (M2-S12 durable
    /// rows). Never counted or timed.
    pub setup: Vec<Vec<Vec<u8>>>,
    /// M4.5-S36 (ADR-0088 D7): an **offered rate** instead of the closed
    /// loop — every connection sends on a fixed schedule
    /// (`conns / target` seconds apart) up to `pipeline` in flight, and
    /// latency is measured from the *intended* send instant, so queueing
    /// behind a slow server counts (coordinated omission is not hidden).
    /// A connection whose pipeline is full skips its slot; the achieved
    /// rate against the target is the report's disclosure. `None` =
    /// closed loop (every pre-S36 row byte-identical).
    pub target_ops_per_sec: Option<u64>,
}

impl Default for LoadSpec {
    fn default() -> LoadSpec {
        LoadSpec {
            host: "127.0.0.1".into(),
            port: 6379,
            conns: 64,
            pipeline: 16,
            duration: Duration::from_secs(10),
            set_weight: 1,
            get_weight: 10,
            keys: 1_000_000,
            key_prefix: "key:".into(),
            key_size: 16,
            value_size: 64,
            seed: 0xC0FFEE,
            warmup: Duration::from_secs(1),
            fill: None,
            ttl_range_ms: None,
            pxat_ms: None,
            setup: Vec::new(),
            target_ops_per_sec: None,
        }
    }
}

/// Distinct error-reply texts retained per report. Bounded so a
/// pathological server cannot balloon a soak leg's memory; 8 distinct
/// strings has always been enough to name every refusal class in play
/// (the 20260807 soak had exactly one across 31 M errors).
const ERROR_SAMPLE_CAP: usize = 8;

#[derive(Clone, Debug, Default)]
pub struct LoadReport {
    pub ops: u64,
    pub errors: u64,
    /// The subset of `errors` that are `-BUSY` typed retryable refusals
    /// (admission backpressure). A leg with only BUSY refusals is a
    /// different fact than one with `-ERR`s — the 20260807 soak's 31 M
    /// unclassified errors took a post-hoc repro to diagnose.
    pub busy_retryable: u64,
    /// RESP nil replies (`$-1`) — client-side GET misses (M4-S20: the
    /// cache-leg hit-rate proxy that no INFO scrape can mix up across
    /// concurrently loaded namespaces).
    pub nils: u64,
    /// First few distinct error-reply lines observed (capped).
    pub error_samples: Vec<String>,
    pub elapsed_s: f64,
    pub ops_per_sec: f64,
    pub p50_us: u64,
    pub p99_us: u64,
    pub p999_us: u64,
    pub p9999_us: u64,
    pub max_us: u64,
}

struct ConnResult {
    ops: u64,
    errors: u64,
    busy: u64,
    nils: u64,
    error_samples: Vec<String>,
    hist_us: LogHistogram,
}

pub(crate) fn make_key(spec: &LoadSpec, index: u64) -> Vec<u8> {
    let digits = spec.key_size.saturating_sub(spec.key_prefix.len()).max(1);
    format!("{}{:0digits$}", spec.key_prefix, index, digits = digits).into_bytes()
}

fn run_conn(
    spec: &LoadSpec,
    conn_index: usize,
    warmup_end: Instant,
    deadline: Instant,
) -> Result<ConnResult, String> {
    let mut stream = connect(&spec.host, spec.port)?;
    for command in &spec.setup {
        let argv: Vec<&[u8]> = command.iter().map(Vec::as_slice).collect();
        let reply = crate::resp::request(&mut stream, &argv)?;
        if reply.starts_with(b"-") {
            return Err(format!(
                "setup command {:?} failed: {}",
                String::from_utf8_lossy(&command[0]),
                String::from_utf8_lossy(&reply)
            ));
        }
    }
    let mut rng = SplitMix64::new(spec.seed ^ (0xB0A7 + conn_index as u64));
    let value = vec![0xABu8; spec.value_size];
    let mut result = ConnResult {
        ops: 0,
        errors: 0,
        busy: 0,
        nils: 0,
        error_samples: Vec::new(),
        hist_us: LogHistogram::new(),
    };

    // Fill mode: a partitioned range, exactly once, pipelined.
    let mut fill_range = spec.fill.map(|total| {
        let per = total / spec.conns as u64;
        let start = per * conn_index as u64;
        let end = if conn_index == spec.conns - 1 { total } else { start + per };
        start..end
    });

    let mut inflight: VecDeque<Instant> = VecDeque::with_capacity(spec.pipeline);
    let mut rx: Vec<u8> = Vec::with_capacity(64 * 1024);
    let mut rx_at = 0usize;
    let mut tx: Vec<u8> = Vec::with_capacity(16 * 1024);
    let mut chunk = [0u8; 64 * 1024];
    let mut done_sending = false;
    // Offered-rate schedule (ADR-0088 D7): this connection's share of
    // the target, staggered by index so the fleet does not send in
    // lockstep; `None` = closed loop.
    let pace = spec.target_ops_per_sec.filter(|t| *t > 0 && spec.fill.is_none()).map(|target| {
        let per_conn = target.max(1) as f64 / spec.conns.max(1) as f64;
        Duration::from_secs_f64(1.0 / per_conn)
    });
    let mut next_send_at = pace.map_or(Instant::now(), |interval| {
        Instant::now() + interval.mul_f64(conn_index as f64 / spec.conns.max(1) as f64)
    });

    loop {
        // Top up the pipeline.
        tx.clear();
        while inflight.len() < spec.pipeline && !done_sending {
            if let Some(interval) = pace
                && fill_range.is_none()
            {
                let now = Instant::now();
                if now >= deadline {
                    done_sending = true;
                    break;
                }
                if now < next_send_at {
                    if inflight.is_empty() {
                        // Nothing to wait on: idle until the slot.
                        #[allow(clippy::disallowed_methods)] // bench pacing, not cell code
                        std::thread::sleep(next_send_at - now);
                    } else {
                        break; // replies first; the slot is still ahead
                    }
                }
                let key = make_key(spec, rng.next_u64() % spec.keys);
                let total = spec.set_weight + spec.get_weight;
                if rng.next_u64() % total < spec.set_weight {
                    tx.extend_from_slice(&encode_command(&[b"SET", &key, &value]));
                } else {
                    tx.extend_from_slice(&encode_command(&[b"GET", &key]));
                }
                // Latency from the *intended* instant (coordinated
                // omission counted), never from the actual send.
                inflight.push_back(next_send_at);
                next_send_at += interval;
                continue;
            }
            match &mut fill_range {
                Some(range) => match range.next() {
                    Some(i) => {
                        let key = make_key(spec, i);
                        if let Some(at) = spec.pxat_ms {
                            let at = at.to_string();
                            tx.extend_from_slice(&encode_command(&[
                                b"SET",
                                &key,
                                &value,
                                b"PXAT",
                                at.as_bytes(),
                            ]));
                        } else {
                            tx.extend_from_slice(&encode_command(&[b"SET", &key, &value]));
                        }
                    }
                    None => {
                        done_sending = true;
                        break;
                    }
                },
                None => {
                    if Instant::now() >= deadline {
                        done_sending = true;
                        break;
                    }
                    let key = make_key(spec, rng.next_u64() % spec.keys);
                    let total = spec.set_weight + spec.get_weight;
                    if rng.next_u64() % total < spec.set_weight {
                        if let Some((lo, hi)) = spec.ttl_range_ms {
                            let px = (lo + rng.next_u64() % (hi - lo).max(1)).to_string();
                            tx.extend_from_slice(&encode_command(&[
                                b"SET",
                                &key,
                                &value,
                                b"PX",
                                px.as_bytes(),
                            ]));
                        } else {
                            tx.extend_from_slice(&encode_command(&[b"SET", &key, &value]));
                        }
                    } else {
                        tx.extend_from_slice(&encode_command(&[b"GET", &key]));
                    }
                }
            }
            inflight.push_back(Instant::now());
        }
        if !tx.is_empty() {
            stream.write_all(&tx).map_err(|e| format!("write: {e}"))?;
        }
        if inflight.is_empty() {
            break; // deadline passed and everything drained
        }

        // Read replies; record latency per completed frame.
        let n = stream.read(&mut chunk).map_err(|e| format!("read: {e}"))?;
        if n == 0 {
            return Err("server closed connection under load".into());
        }
        rx.extend_from_slice(&chunk[..n]);
        while let Some(end) = reply_len(&rx[rx_at..]) {
            let sent = inflight.pop_front().ok_or("reply without a request")?;
            if sent >= warmup_end {
                let micros = sent.elapsed().as_micros() as u64;
                result.hist_us.record(micros);
                result.ops += 1;
                if rx[rx_at..].starts_with(b"$-1") {
                    result.nils += 1;
                }
                // Errors count under the same warmup guard as ops, so an
                // error *rate* is errors/ops over one window (pre-fix the
                // first leg's rate was inflated by warmup-only errors).
                if rx[rx_at] == b'-' {
                    result.errors += 1;
                    let line = &rx[rx_at..rx_at + end];
                    if line.starts_with(b"-BUSY") {
                        result.busy += 1;
                    }
                    if result.error_samples.len() < ERROR_SAMPLE_CAP {
                        let text = String::from_utf8_lossy(line).trim_end().to_string();
                        if !result.error_samples.contains(&text) {
                            result.error_samples.push(text);
                        }
                    }
                }
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
    Ok(result)
}

/// Runs the load and merges per-connection results.
pub fn run(spec: &LoadSpec) -> Result<LoadReport, String> {
    let started = Instant::now();
    let warmup = if spec.fill.is_some() { Duration::ZERO } else { spec.warmup };
    let warmup_end = started + warmup;
    let deadline = warmup_end + spec.duration;
    let results: Vec<Result<ConnResult, String>> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..spec.conns)
            .map(|i| scope.spawn(move || run_conn(spec, i, warmup_end, deadline)))
            .collect();
        handles.into_iter().map(|h| h.join().expect("load conn thread")).collect()
    });
    let elapsed = started.elapsed().saturating_sub(warmup);

    let mut report = LoadReport { elapsed_s: elapsed.as_secs_f64(), ..Default::default() };
    let mut hist = LogHistogram::new();
    for result in results {
        let conn = result?;
        report.ops += conn.ops;
        report.errors += conn.errors;
        report.busy_retryable += conn.busy;
        report.nils += conn.nils;
        for sample in conn.error_samples {
            if report.error_samples.len() < ERROR_SAMPLE_CAP
                && !report.error_samples.contains(&sample)
            {
                report.error_samples.push(sample);
            }
        }
        hist.merge(&conn.hist_us);
    }
    report.ops_per_sec = report.ops as f64 / report.elapsed_s;
    report.p50_us = hist.percentile(50.0);
    report.p99_us = hist.percentile(99.0);
    report.p999_us = hist.percentile(99.9);
    report.p9999_us = hist.percentile(99.99);
    report.max_us = hist.max();
    Ok(report)
}

pub fn render(report: &LoadReport) -> String {
    let mut out = format!(
        "ops = {}\nerrors = {}\nbusy_retryable = {}\nelapsed_s = {:.3}\nops_per_sec = {:.0}\n\
         p50_us = {}\np99_us = {}\np999_us = {}\np9999_us = {}\nmax_us = {}\n",
        report.ops,
        report.errors,
        report.busy_retryable,
        report.elapsed_s,
        report.ops_per_sec,
        report.p50_us,
        report.p99_us,
        report.p999_us,
        report.p9999_us,
        report.max_us
    );
    for sample in &report.error_samples {
        out.push_str(&format!("error_sample = {sample}\n"));
    }
    out
}

/// `inf-bench load` CLI.
pub fn cmd_load(args: &[String]) -> Result<(), String> {
    let flags = Flags::parse(
        args,
        &[],
        &[
            "host",
            "port",
            "conns",
            "pipeline",
            "duration",
            "mix",
            "keys",
            "key-prefix",
            "key-size",
            "value-size",
            "seed",
            "fill",
            "out",
            // M2-S22: one space-separated command every connection sends
            // (error-checked, untimed) before the load — e.g.
            // `--setup "INF.NS USE soak_es"` for durable-namespace legs.
            "setup",
        ],
    )?;
    let mut spec = LoadSpec::default();
    spec.host = flags.str_or("host", &spec.host);
    if let Some(v) = flags.get("port") {
        spec.port = v.parse().map_err(|e| format!("--port: {e}"))?;
    }
    if let Some(v) = flags.get("conns") {
        spec.conns = v.parse().map_err(|e| format!("--conns: {e}"))?;
    }
    if let Some(v) = flags.get("pipeline") {
        spec.pipeline = v.parse().map_err(|e| format!("--pipeline: {e}"))?;
    }
    if let Some(v) = flags.get("duration") {
        spec.duration = Duration::from_secs(v.parse().map_err(|e| format!("--duration: {e}"))?);
    }
    if let Some(v) = flags.get("mix") {
        let (set, get) = v.split_once(':').ok_or("--mix wants SET:GET, e.g. 1:10")?;
        spec.set_weight = set.parse().map_err(|e| format!("--mix: {e}"))?;
        spec.get_weight = get.parse().map_err(|e| format!("--mix: {e}"))?;
    }
    if let Some(v) = flags.get("keys") {
        spec.keys = v.parse().map_err(|e| format!("--keys: {e}"))?;
    }
    spec.key_prefix = flags.str_or("key-prefix", &spec.key_prefix);
    if let Some(v) = flags.get("key-size") {
        spec.key_size = v.parse().map_err(|e| format!("--key-size: {e}"))?;
    }
    if let Some(v) = flags.get("value-size") {
        spec.value_size = v.parse().map_err(|e| format!("--value-size: {e}"))?;
    }
    if let Some(v) = flags.get("seed") {
        spec.seed = v.parse().map_err(|e| format!("--seed: {e}"))?;
    }
    if let Some(v) = flags.get("fill") {
        spec.fill = Some(v.parse().map_err(|e| format!("--fill: {e}"))?);
    }
    if let Some(v) = flags.get("setup") {
        let cmd: Vec<Vec<u8>> = v.split_whitespace().map(|w| w.as_bytes().to_vec()).collect();
        if cmd.is_empty() {
            return Err("--setup: empty command".into());
        }
        spec.setup = vec![cmd];
    }

    let report = run(&spec)?;
    let rendered = render(&report);
    print!("{rendered}");
    if let Some(path) = flags.get("out") {
        std::fs::write(path, rendered).map_err(|e| format!("--out {path}: {e}"))?;
    }
    if report.errors > 0 {
        // Keep the "error replies under load" prefix stable — soak
        // tooling greps for it. The classification rides behind it.
        let first = report.error_samples.first().map(String::as_str).unwrap_or("none sampled");
        return Err(format!(
            "{} error replies under load ({} BUSY-retryable; first sample: {})",
            report.errors, report.busy_retryable, first
        ));
    }
    Ok(())
}
