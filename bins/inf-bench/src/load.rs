//! Native pipelined RESP load generator (M0-S18): N blocking connections on
//! N threads, fixed pipeline depth, seeded SET/GET mix, per-command latency
//! into merged `FineHistogram`s (256 sub-buckets/octave ≈ 0.4 % — the
//! 2026-08-22 instrument; before it the kernel's 3 % `LogHistogram`). Also
//! the deterministic fill mode (each connection SETs a partitioned key
//! range exactly once) for the RSS gate.

use std::collections::VecDeque;
use std::io::{ErrorKind, Read, Write};
use std::time::{Duration, Instant};

use crate::finehist::FineHistogram;
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
    /// latency is measured from the *intended* send instant, so a late
    /// wake-up counts as queueing (coordinated omission is not hidden).
    /// A slot that comes due while the connection's pipeline is full is
    /// **skipped, never sent late** (M4.5-S40 review, 2026-08-25: the
    /// first implementation caught up after a stall — a burst above the
    /// offered rate whose latencies were stamped from slots long past);
    /// the report counts `offered` / `sent` / `skipped_pipeline_full`
    /// and the achieved rate against the target is its disclosure.
    /// `None` = closed loop (every pre-S36 row byte-identical).
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
    /// Exact mean of the leg's latencies — disclosed beside the
    /// percentiles (never a gate input on its own).
    pub mean_us: f64,
    /// M4.5-S40 (stall attribution): the `max_us` sample's three
    /// instants in seconds after warmup — its *intended* send (the slot,
    /// what the latency is measured from), its *actual* send, and its
    /// completion — so a server-side timeline is read over the interval
    /// the request was really outstanding, not a window around one
    /// stamp; and the per-second maxima over the leg (index = whole
    /// seconds after the warmup, by actual send), so an isolated event
    /// is seen as one.
    pub max_intended_at_s: f64,
    pub max_sent_at_s: f64,
    pub max_done_at_s: f64,
    pub max_per_second: Vec<u64>,
    /// Offered-rate accounting inside the measured window (0 on a closed
    /// loop): schedule slots offered = `sent + skipped_pipeline_full`;
    /// a skipped slot is one that came due while the connection's
    /// pipeline was full (the request was never sent — the achieved rate
    /// falls by it, no later request carries its wait).
    pub offered: u64,
    pub sent: u64,
    pub skipped_pipeline_full: u64,
}

/// One in-flight request's instants: the schedule slot it was sent for
/// (latency counts from here) and when it actually left. Equal on a
/// closed loop.
#[derive(Copy, Clone, Debug)]
struct SentAt {
    intended: Instant,
    sent: Instant,
}

/// Whole schedule slots in `[from, now]` — the slots a full pipeline let
/// pass; at least one when `now >= from`.
fn slots_elapsed(from: Instant, now: Instant, interval: Duration) -> u64 {
    debug_assert!(now >= from);
    (now.duration_since(from).as_nanos() / interval.as_nanos().max(1)) as u64 + 1
}

/// How many of `count` consecutive slots starting at `first` fall at or
/// after `window_start` — the skipped slots inside the measured window.
fn slots_in_window(first: Instant, interval: Duration, count: u64, window_start: Instant) -> u64 {
    if first >= window_start {
        return count;
    }
    let before = window_start.duration_since(first).as_nanos().div_ceil(interval.as_nanos().max(1));
    count.saturating_sub(before as u64)
}

struct ConnResult {
    ops: u64,
    errors: u64,
    busy: u64,
    nils: u64,
    error_samples: Vec<String>,
    hist_us: FineHistogram,
    max_us: u64,
    max_intended_at_s: f64,
    max_sent_at_s: f64,
    max_done_at_s: f64,
    max_per_second: Vec<u64>,
    sent: u64,
    skipped_pipeline_full: u64,
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
        hist_us: FineHistogram::new(),
        max_us: 0,
        max_intended_at_s: 0.0,
        max_sent_at_s: 0.0,
        max_done_at_s: 0.0,
        max_per_second: vec![0; spec.duration.as_secs() as usize + 2],
        sent: 0,
        skipped_pipeline_full: 0,
    };

    // Fill mode: a partitioned range, exactly once, pipelined.
    let mut fill_range = spec.fill.map(|total| {
        let per = total / spec.conns as u64;
        let start = per * conn_index as u64;
        let end = if conn_index == spec.conns - 1 { total } else { start + per };
        start..end
    });

    let mut inflight: VecDeque<SentAt> = VecDeque::with_capacity(spec.pipeline);
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
    // Set when the sender stopped on a full pipeline: every slot that
    // comes due before a reply frees it is skipped, never sent late
    // (the contract — a catch-up burst exceeds the offered rate and
    // stamps the skipped slots' wait onto requests sent afterwards).
    let mut blocked_full = false;

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
                if blocked_full {
                    blocked_full = false;
                    if now >= next_send_at {
                        let missed = slots_elapsed(next_send_at, now, interval);
                        result.skipped_pipeline_full +=
                            slots_in_window(next_send_at, interval, missed, warmup_end);
                        next_send_at += Duration::from_nanos(interval.as_nanos() as u64 * missed);
                    }
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
                // Latency from the *intended* instant (a late wake-up
                // is queueing and counts); the actual instant rides
                // beside it for the timeline a maximum is read against.
                let intended = next_send_at;
                inflight.push_back(SentAt { intended, sent: Instant::now() });
                if intended >= warmup_end {
                    result.sent += 1;
                }
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
            let now = Instant::now();
            inflight.push_back(SentAt { intended: now, sent: now });
        }
        if !tx.is_empty() {
            stream.write_all(&tx).map_err(|e| format!("write: {e}"))?;
        }
        if inflight.is_empty() {
            break; // deadline passed and everything drained
        }

        // Read replies; record latency per completed frame. On the
        // offered schedule a pipeline with room reads only until its
        // next slot is due (a reply is not what the schedule waits
        // for); a full pipeline reads until a reply frees a slot, and
        // the slots that come due meanwhile are skipped above.
        let full = inflight.len() >= spec.pipeline;
        blocked_full = full && pace.is_some() && !done_sending;
        let wait = if full || done_sending || pace.is_none() {
            None
        } else {
            Some(
                next_send_at
                    .saturating_duration_since(Instant::now())
                    .max(Duration::from_micros(1)),
            )
        };
        stream.set_read_timeout(wait).map_err(|e| format!("read timeout: {e}"))?;
        let n = match stream.read(&mut chunk) {
            Ok(n) => n,
            Err(e)
                if wait.is_some()
                    && matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
            {
                continue; // the slot is due; send, then read again
            }
            Err(e) => return Err(format!("read: {e}")),
        };
        if n == 0 {
            return Err("server closed connection under load".into());
        }
        rx.extend_from_slice(&chunk[..n]);
        while let Some(end) = reply_len(&rx[rx_at..]) {
            let SentAt { intended, sent } =
                inflight.pop_front().ok_or("reply without a request")?;
            if intended >= warmup_end {
                let done = Instant::now();
                let micros = done.duration_since(intended).as_micros() as u64;
                result.hist_us.record(micros);
                result.ops += 1;
                // `sent >= intended >= warmup_end`: a slot is sent at or
                // after its instant, never before.
                let since = sent.duration_since(warmup_end);
                if micros > result.max_us {
                    result.max_us = micros;
                    result.max_intended_at_s = intended.duration_since(warmup_end).as_secs_f64();
                    result.max_sent_at_s = since.as_secs_f64();
                    result.max_done_at_s = done.duration_since(warmup_end).as_secs_f64();
                }
                if let Some(slot) = result.max_per_second.get_mut(since.as_secs() as usize) {
                    *slot = (*slot).max(micros);
                }
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
    let mut hist = FineHistogram::new();
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
        report.sent += conn.sent;
        report.skipped_pipeline_full += conn.skipped_pipeline_full;
        if conn.max_us > report.max_us || report.max_per_second.is_empty() {
            report.max_us = conn.max_us;
            report.max_intended_at_s = conn.max_intended_at_s;
            report.max_sent_at_s = conn.max_sent_at_s;
            report.max_done_at_s = conn.max_done_at_s;
        }
        if report.max_per_second.len() < conn.max_per_second.len() {
            report.max_per_second.resize(conn.max_per_second.len(), 0);
        }
        for (slot, m) in report.max_per_second.iter_mut().zip(&conn.max_per_second) {
            *slot = (*slot).max(*m);
        }
    }
    report.offered = report.sent + report.skipped_pipeline_full;
    report.ops_per_sec = report.ops as f64 / report.elapsed_s;
    report.p50_us = hist.percentile(50.0);
    report.p99_us = hist.percentile(99.0);
    report.p999_us = hist.percentile(99.9);
    report.p9999_us = hist.percentile(99.99);
    report.max_us = hist.max();
    report.mean_us = hist.mean();
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
    if report.offered > 0 {
        out.push_str(&format!(
            "offered = {}\nsent = {}\nskipped_pipeline_full = {}\n",
            report.offered, report.sent, report.skipped_pipeline_full
        ));
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The skip rule's arithmetic: a pipeline freed `now` skips every
    /// slot from the one that was due up to `now` (inclusive of a slot
    /// exactly at `now`), and only the slots inside the measured window
    /// are counted.
    #[test]
    fn skipped_slots_are_counted_from_the_due_slot_and_inside_the_window() {
        let interval = Duration::from_micros(320);
        let t0 = Instant::now();
        // Due 1 ms ago: slots at 0, 320, 640, 960 µs have passed — four.
        assert_eq!(slots_elapsed(t0, t0 + Duration::from_micros(1_000), interval), 4);
        // Due exactly now: one slot.
        assert_eq!(slots_elapsed(t0, t0, interval), 1);
        // Window starts at 500 µs: of the four slots, 640 and 960 are in it.
        assert_eq!(slots_in_window(t0, interval, 4, t0 + Duration::from_micros(500)), 2);
        // Window starts at the first slot or before it: all of them.
        assert_eq!(slots_in_window(t0, interval, 4, t0), 4);
        // Window starts after every slot: none.
        assert_eq!(slots_in_window(t0, interval, 4, t0 + Duration::from_secs(1)), 0);
    }
}
