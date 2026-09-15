//! M4.5-S40 rows: the write-stall attribution row.

use super::*;

// ---- M4.5-S40: stall attribution at the memtier shape (S27 D5 `max`) ---------

/// Keys of the S40 memtier shape (`--keyspace 1000000`): random SETs
/// over 1 M keys × 1 KiB grow the dataset to ~1 GB inside the leg, so
/// the derived checkpoint trigger fires several times per minute at the
/// product's floor — the checkpoint phases the 103 ms maximum may sit in.
const S40_DEFAULT_KEYS: u64 = 1_000_000;

/// One server-side timeline sample (every cell summed / maxed) taken
/// every `S40_SAMPLE_MS` during the leg — the events a client maximum is
/// read against.
#[derive(Clone, Debug, Default)]
struct S40Sample {
    at_s: f64,
    /// Cells with a checkpoint stream open (`ckpt_buffer_bytes > 0`).
    ckpt_in_flight: u64,
    ckpts_completed: u64,
    ckpt_bytes: u64,
    manifests_published: u64,
    truncated: u64,
    rotations: u64,
    zero_fill: u64,
    parked: u64,
    stall_p99_us: u64,
    stall_p999_us: u64,
    waits_barrier: u64,
    waits_rotation: u64,
    waits_pace: u64,
    ckpt_deferrals: u64,
    /// Stop-and-copy index grows (`INFO stats index_grows`, summed over
    /// cells): the deterministic 12–17 ms maxima campaign I saw at the
    /// same seconds of every leg.
    index_grows: u64,
    /// `/sys/block/<dev>/stat`: sectors written (7), ms writing (8),
    /// ms doing I/O (10) — 0 when not sampled.
    dev_sectors_written: u64,
    dev_ms_writing: u64,
    dev_io_ms: u64,
}

const S40_SAMPLE_MS: u64 = 250;

fn s40_sample(port: u16, cells: u16, device_stat: Option<&str>, t0: Instant) -> Option<S40Sample> {
    let infos = scrape_cells(port, cells).ok()?;
    let per = |f: &str| -> Vec<u64> {
        infos.iter().map(|c| c.get(f).and_then(|v| v.parse().ok()).unwrap_or(0)).collect()
    };
    let dev =
        device_stat.and_then(|d| std::fs::read_to_string(format!("/sys/block/{d}/stat")).ok());
    let dev_field = |i: usize| -> u64 {
        dev.as_deref()
            .and_then(|s| s.split_whitespace().nth(i).and_then(|v| v.parse().ok()))
            .unwrap_or(0)
    };
    Some(S40Sample {
        at_s: t0.elapsed().as_secs_f64(),
        ckpt_in_flight: per("ckpt_buffer_bytes").iter().filter(|&&b| b > 0).count() as u64,
        ckpts_completed: sum_field(&infos, "ckpts_completed"),
        ckpt_bytes: sum_field(&infos, "ckpt_bytes_total"),
        manifests_published: sum_field(&infos, "manifests_published"),
        truncated: sum_field(&infos, "segments_truncated"),
        rotations: sum_field(&infos, "segment_rotations"),
        zero_fill: sum_field(&infos, "zero_fill_bytes"),
        parked: sum_field(&infos, "log_admission_parked_total"),
        stall_p99_us: crate::gaterun::max_field(&infos, "log_write_stall_p99_us"),
        stall_p999_us: crate::gaterun::max_field(&infos, "log_write_stall_p999_us"),
        waits_barrier: sum_field(&infos, "frame_waits_barrier"),
        waits_rotation: sum_field(&infos, "frame_waits_rotation"),
        waits_pace: sum_field(&infos, "frame_waits_pace"),
        ckpt_deferrals: sum_field(&infos, "io_budget_deferrals_checkpoint"),
        index_grows: sum_field(&infos, "index_grows"),
        dev_sectors_written: dev_field(6),
        dev_ms_writing: dev_field(7),
        dev_io_ms: dev_field(9),
    })
}

/// What the timeline says happened over the interval that brackets the
/// client's maximum (its actual send to its completion): every engine
/// event is named, the device's write time over the interval disclosed,
/// and **every** candidate class present is listed (checkpoint,
/// rotation, manifest/truncation, zero-fill, index grow, device-busy
/// ≥ 50 %, admission park — in that order, `+`-joined), or
/// `unattributed`. A list, never a verdict: the precedence the first
/// instrument chose one word by hid a co-occurring cause (review of
/// campaigns I/I2, 2026-08-25).
fn s40_attribute(before: &S40Sample, after: &S40Sample) -> (String, String) {
    let window_ms = ((after.at_s - before.at_s) * 1000.0).max(1.0);
    let d = |f: fn(&S40Sample) -> u64| f(after).saturating_sub(f(before));
    let mut events = Vec::new();
    if before.ckpt_in_flight > 0 || after.ckpt_in_flight > 0 {
        events.push(format!(
            "checkpoint in flight ({}→{} cells, +{} bytes)",
            before.ckpt_in_flight,
            after.ckpt_in_flight,
            d(|s| s.ckpt_bytes)
        ));
    }
    if d(|s| s.ckpts_completed) > 0 {
        events.push(format!("checkpoint published (+{})", d(|s| s.ckpts_completed)));
    }
    if d(|s| s.rotations) > 0 {
        events.push(format!("rotation (+{})", d(|s| s.rotations)));
    }
    if d(|s| s.manifests_published) > 0 || d(|s| s.truncated) > 0 {
        events.push(format!(
            "manifest/truncation (+{} manifests, +{} segments)",
            d(|s| s.manifests_published),
            d(|s| s.truncated)
        ));
    }
    if d(|s| s.zero_fill) > 0 {
        events.push(format!("zero-fill (+{} bytes)", d(|s| s.zero_fill)));
    }
    if d(|s| s.parked) > 0 {
        events.push(format!("admission parks (+{})", d(|s| s.parked)));
    }
    if d(|s| s.ckpt_deferrals) > 0 {
        events.push(format!("checkpoint offers deferred (+{})", d(|s| s.ckpt_deferrals)));
    }
    if d(|s| s.index_grows) > 0 {
        events.push(format!("index grow, stop-and-copy (+{})", d(|s| s.index_grows)));
    }
    if d(|s| s.waits_rotation) > 0 || d(|s| s.waits_barrier) > 0 || d(|s| s.waits_pace) > 0 {
        events.push(format!(
            "frame waits (+{} barrier, +{} rotation, +{} pace)",
            d(|s| s.waits_barrier),
            d(|s| s.waits_rotation),
            d(|s| s.waits_pace)
        ));
    }
    let dev_busy_pct = d(|s| s.dev_io_ms) as f64 * 100.0 / window_ms;
    let dev_note = if after.dev_sectors_written > 0 {
        format!(
            "device: +{} MiB written, {} ms writing, io busy {:.0} % of the {:.0} ms window",
            (d(|s| s.dev_sectors_written) * 512) >> 20,
            d(|s| s.dev_ms_writing),
            dev_busy_pct,
            window_ms
        )
    } else {
        "device: not sampled".to_string()
    };
    let mut candidates: Vec<&str> = Vec::new();
    if before.ckpt_in_flight > 0 || after.ckpt_in_flight > 0 || d(|s| s.ckpts_completed) > 0 {
        candidates.push("checkpoint");
    }
    if d(|s| s.rotations) > 0 {
        candidates.push("rotation");
    }
    if d(|s| s.manifests_published) > 0 || d(|s| s.truncated) > 0 {
        candidates.push("manifest/truncation");
    }
    if d(|s| s.zero_fill) > 0 {
        candidates.push("zero-fill");
    }
    if d(|s| s.index_grows) > 0 {
        candidates.push("index-grow");
    }
    if dev_busy_pct >= 50.0 {
        candidates.push("device-busy");
    }
    if d(|s| s.parked) > 0 {
        // Campaign I's lesson (2026-08-23): a park is the staging domain
        // filling behind frames the device is not completing — listed
        // beside `device-busy`, never instead of it.
        candidates.push("admission-park");
    }
    let word =
        if candidates.is_empty() { "unattributed".to_string() } else { candidates.join("+") };
    let detail = if events.is_empty() {
        format!("no engine event in the interval; {dev_note}")
    } else {
        format!("{}; {dev_note}", events.join(", "))
    };
    (word, detail)
}

/// One S40 leg's facts.
struct S40Leg {
    ops_per_sec: f64,
    p50_us: f64,
    p99_us: f64,
    p999_us: f64,
    max_us: f64,
    /// The maximum's intended send (its schedule slot), actual send and
    /// completion, seconds after the warmup — the attribution interval
    /// is `[sent, done]`, never one window around a stamp.
    max_intended_at_s: f64,
    max_sent_at_s: f64,
    max_done_at_s: f64,
    /// Offered-rate accounting (generator contract, ADR-0088 D7 as
    /// corrected 2026-08-25): slots offered = sent + skipped on a full
    /// pipeline; a skipped slot lowers the achieved rate and is never
    /// sent late.
    offered: u64,
    sent: u64,
    skipped_pipeline_full: u64,
    /// Seconds whose maximum exceeded 50 ms, and the top per-second maxima.
    seconds_over_50ms: u64,
    top_seconds: Vec<(usize, u64)>,
    cpu_pct: f64,
    attribution: String,
    attribution_detail: String,
    ckpts: u64,
    ckpt_bytes: u64,
    stall_p99_us: u64,
    stall_p999_us: u64,
    parked: u64,
    dev_mib: u64,
}

/// The M4.5-S40 stall-attribution leg: the memtier shape on the in-house
/// generator (1 M keys × 1 KiB, 32 conns, pipeline 1, `everysec`, an
/// offered rate with latency from the intended send), the server's
/// counters and the block device sampled every 250 ms across it, and
/// the client's maximum read against the sample window it fell in.
#[allow(clippy::too_many_arguments)]
fn s40_leg(
    flags: &Flags,
    infinityd: &str,
    cells: u16,
    duration: u64,
    offered: u64,
    keys: u64,
    dir: &str,
    device_stat: Option<&str>,
) -> Result<S40Leg, String> {
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).map_err(|e| format!("{dir}: {e}"))?;
    crate::m2rows::copy_probe_file(flags, std::path::Path::new(dir))?;
    let mut extra: Vec<String> = vec!["--data-dir".into(), dir.to_string()];
    if let Some(pin) = flags.get("pin-start") {
        extra.push("--pin-start".into());
        extra.push(pin.to_string());
    }
    extra.extend(crate::m2rows::pipeline_args(flags));
    let extra_refs: Vec<&str> = extra.iter().map(String::as_str).collect();
    let server = spawn_infinityd(infinityd, cells, &extra_refs)?;
    let port = server.port;
    create_ns(
        port,
        &[b"INF.NS", b"CREATE", b"s40esec", b"MODE", b"durable", b"FSYNC", b"everysec"],
    )?;
    await_fan(port, "s40esec", cells)?;
    let warmup = Duration::from_secs(2);
    let spec = LoadSpec {
        port,
        conns: 32,
        pipeline: 1,
        duration: Duration::from_secs(duration),
        warmup,
        set_weight: 1,
        get_weight: 0,
        keys,
        key_prefix: "s40:".into(),
        value_size: 1024,
        setup: vec![vec![b"INF.NS".to_vec(), b"USE".to_vec(), b"s40esec".to_vec()]],
        target_ops_per_sec: Some(offered),
        ..LoadSpec::default()
    };
    let ticks_before = crate::gaterun::cpu_ticks_of(server.pid());
    let t0 = Instant::now();
    let load = std::thread::spawn(move || run_load(&spec));
    let mut timeline: Vec<S40Sample> = Vec::new();
    while !load.is_finished() {
        if let Some(s) = s40_sample(port, cells, device_stat, t0) {
            timeline.push(s);
        }
        #[allow(clippy::disallowed_methods)] // bench orchestration, not cell code
        std::thread::sleep(Duration::from_millis(S40_SAMPLE_MS));
    }
    let wall = t0.elapsed().as_secs_f64().max(1e-9);
    let ticks = crate::gaterun::cpu_ticks_of(server.pid()).saturating_sub(ticks_before);
    let report = load.join().map_err(|_| "s40: load thread panicked".to_string())??;
    if report.errors > report.busy_retryable {
        return Err(format!(
            "s40: {} non-BUSY errors (first: {:?})",
            report.errors - report.busy_retryable,
            report.error_samples.first()
        ));
    }
    let (first, last) = match (timeline.first(), timeline.last()) {
        (Some(f), Some(l)) => (f.clone(), l.clone()),
        _ => return Err("s40: no timeline sample".into()),
    };
    // The max's instants are measured from warmup's end; the timeline
    // from the leg's start. The attribution interval is the request's
    // whole outstanding life — the last sample at or before its actual
    // send to the first sample at or after its completion — never one
    // window around a stamp (a 2 s stall spans eight windows; review of
    // campaigns I/I2, 2026-08-25).
    let warmup_s = warmup.as_secs_f64();
    let sent_at_leg = report.max_sent_at_s + warmup_s;
    let done_at_leg = report.max_done_at_s + warmup_s;
    let last_idx = timeline.len() - 1;
    let before_idx = timeline.iter().rposition(|s| s.at_s <= sent_at_leg).unwrap_or(0);
    let after_idx = timeline
        .iter()
        .position(|s| s.at_s >= done_at_leg)
        .unwrap_or(last_idx)
        .max((before_idx + 1).min(last_idx));
    let (attribution, attribution_detail) =
        s40_attribute(&timeline[before_idx], &timeline[after_idx]);
    let mut top: Vec<(usize, u64)> = report.max_per_second.iter().copied().enumerate().collect();
    top.sort_by_key(|&(_, m)| std::cmp::Reverse(m));
    top.truncate(5);
    drop(server);
    Ok(S40Leg {
        ops_per_sec: report.ops_per_sec,
        p50_us: report.p50_us as f64,
        p99_us: report.p99_us as f64,
        p999_us: report.p999_us as f64,
        max_us: report.max_us as f64,
        max_intended_at_s: report.max_intended_at_s,
        max_sent_at_s: report.max_sent_at_s,
        max_done_at_s: report.max_done_at_s,
        offered: report.offered,
        sent: report.sent,
        skipped_pipeline_full: report.skipped_pipeline_full,
        seconds_over_50ms: report.max_per_second.iter().filter(|&&m| m > 50_000).count() as u64,
        top_seconds: top,
        cpu_pct: ticks as f64 / crate::gaterun::CLOCK_TICKS_PER_S as f64 / wall * 100.0,
        attribution,
        attribution_detail,
        ckpts: last.ckpts_completed.saturating_sub(first.ckpts_completed),
        ckpt_bytes: last.ckpt_bytes.saturating_sub(first.ckpt_bytes),
        stall_p99_us: last.stall_p99_us,
        stall_p999_us: last.stall_p999_us,
        parked: last.parked.saturating_sub(first.parked),
        dev_mib: (last.dev_sectors_written.saturating_sub(first.dev_sectors_written) * 512) >> 20,
    })
}

/// The M4.5-S40 stall-attribution row: `--replicates` legs of the memtier
/// shape on the in-house generator, the client maximum of each read
/// against the server/device timeline window it fell in. Reports the
/// worst and median maximum (the S27 D5 `max ≤ 50 ms` wording at this
/// shape), the seconds over 50 ms per leg, and the attribution word per
/// leg — a note, never a gate.
pub(super) fn s40_stall_row(
    flags: &Flags,
    infinityd: &str,
    cells: u16,
    duration: u64,
    replicates: usize,
    data_root: &str,
    m: &mut Measurements,
) -> Result<(), String> {
    let idle_s = flags.u64_or("leg-idle-s", S35_LEG_IDLE_S)?;
    let offered = flags.u64_or("offered-ops", 100_000)?;
    let keys = flags.u64_or("s40-keys", S40_DEFAULT_KEYS)?;
    let device_stat = flags.get("device-stat").map(str::to_string);
    m.note(format!(
        "s40 row: {cells} cells · {replicates} legs · memtier shape on the in-house generator: \
         {keys} keys × 1 KiB, 32 conns, pipeline 1, everysec, {offered} offered ops/s for \
         {duration} s (latency from the intended send; a slot due on a full pipeline is \
         skipped and counted, never sent late) · INFO + device sampled every {S40_SAMPLE_MS} \
         ms · {idle_s} s idle before every leg · device stat {}",
        device_stat.as_deref().unwrap_or("(not sampled)")
    ));
    let mut raw = String::new();
    let mut maxes = Vec::new();
    let mut achieved = Vec::new();
    let mut p99s = Vec::new();
    let mut p999s = Vec::new();
    let mut over = Vec::new();
    let mut words: Vec<String> = Vec::new();
    for rep in 0..replicates {
        let dir = format!("{data_root}/s40-rep{rep}");
        s35_idle(idle_s, &format!("s40 rep{rep}"));
        let leg = s40_leg(
            flags,
            infinityd,
            cells,
            duration,
            offered,
            keys,
            &dir,
            device_stat.as_deref(),
        )?;
        raw.push_str(&format!(
            "rep{rep} achieved={:.0} ({:.3} of offered) offered={} sent={} \
             skipped_pipeline_full={} p50_us={:.0} p99_us={:.0} p999_us={:.0} max_us={:.0} \
             max_intended_at_s={:.3} max_sent_at_s={:.3} max_done_at_s={:.3} \
             seconds_over_50ms={} top_seconds={:?} cpu_pct={:.0} ckpts={} ckpt_bytes={} \
             stall_p99_us={} stall_p999_us={} parked={} dev_mib={} attribution={} [{}]\n",
            leg.ops_per_sec,
            leg.ops_per_sec / offered.max(1) as f64,
            leg.offered,
            leg.sent,
            leg.skipped_pipeline_full,
            leg.p50_us,
            leg.p99_us,
            leg.p999_us,
            leg.max_us,
            leg.max_intended_at_s,
            leg.max_sent_at_s,
            leg.max_done_at_s,
            leg.seconds_over_50ms,
            leg.top_seconds,
            leg.cpu_pct,
            leg.ckpts,
            leg.ckpt_bytes,
            leg.stall_p99_us,
            leg.stall_p999_us,
            leg.parked,
            leg.dev_mib,
            leg.attribution,
            leg.attribution_detail,
        ));
        println!("  s40 rep{rep}: {}", raw.lines().last().unwrap_or(""));
        let _ = std::fs::remove_dir_all(&dir);
        if leg.skipped_pipeline_full > 0 {
            m.note(format!(
                "s40 rep{rep}: {} of {} offered slots skipped on a full pipeline ({:.4} share) \
                 — never sent late; the achieved rate carries them",
                leg.skipped_pipeline_full,
                leg.offered,
                leg.skipped_pipeline_full as f64 / leg.offered.max(1) as f64
            ));
        }
        maxes.push(leg.max_us / 1000.0);
        achieved.push(leg.ops_per_sec / offered.max(1) as f64);
        p99s.push(leg.p99_us);
        p999s.push(leg.p999_us);
        over.push(leg.seconds_over_50ms as f64);
        words.push(leg.attribution);
    }
    m.set("s40:max_ms_worst", maxes.iter().copied().fold(0.0, f64::max));
    m.set("s40:max_ms_median", median(&mut maxes));
    m.set("s40:offered_rate_achieved_x_min", achieved.iter().copied().fold(f64::MAX, f64::min));
    m.set("s40:p99_us_median", median(&mut p99s));
    m.set("s40:p999_us_median", median(&mut p999s));
    m.set("s40:seconds_over_50ms_total", over.iter().sum());
    m.note(format!(
        "s40 attribution candidates per leg (over the max's send-to-completion interval): {}",
        words.join(" / ")
    ));
    if achieved.iter().any(|a| *a < 0.9) {
        m.note("s40: a leg achieved < 0.90 of the offered rate — its max is a saturation number");
    }
    m.row_open("stall-attribution");
    m.row_write_amp(
        "S40 stall attribution: the client's maximum is read against the 250 ms server/device \
         samples spanning its actual send to its completion; every candidate class present in \
         that interval is listed (checkpoint, rotation, manifest/truncation, zero-fill, index \
         grow, device busy ≥ 50 %, admission park — `+`-joined) or `unattributed`; a note, \
         never a gate. Offered slots that came due on a full pipeline are skipped, counted, and \
         never sent late (the corrected ADR-0088 D7 generator, 2026-08-25)",
    );
    m.raw_section("s40 per-leg samples", &raw);
    Ok(())
}
