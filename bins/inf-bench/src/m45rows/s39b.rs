//! M4.5-S39b rows: segment recycling under the pool wait (ADR-0090).

use super::*;

// ---- M4.5-S39b: segment recycling (ADR-0090 D6 as amended) -----------------

/// Default closed-loop write-leg length of the S39b row (seconds): at
/// ~18–25 MB/s of frames per cell and the product's 256 MiB segments
/// it yields ≥ 8 rotations per cell after the first generation
/// (ADR-0090 D6: "the row runs 150 s"); `--duration` overrides.
pub(super) const S39B_DEFAULT_DURATION_S: u64 = 150;

/// Segment size the S39b row runs at unless `--segment-bytes` says
/// otherwise: the product default. Smaller segments do not measure the
/// mechanism at this row's dataset (ADR-0090 A5): the derived checkpoint
/// trigger is `max(2 × dataset, floor)` ≈ 100 MB per cell here, so an
/// 8 MiB segment sees truncation in bursts of ~12 per checkpoint and a
/// 1-slot pool serves one of them (the smoke read 0.64 warmed share) —
/// a row fact, not a mechanism fact. `--ckpt-interval-bytes` defaults
/// to the segment size (the floor; the α term binds either way).
// = inf_log::DEFAULT_SEGMENT_BYTES (inf-bench is zero-dep)
pub(super) const S39B_DEFAULT_SEGMENT_BYTES: u64 = 256 << 20;

pub(super) fn s39b_snap(
    port: u16,
    cells: u16,
    device_stat: Option<&str>,
    t0: Instant,
) -> Result<S39bSnap, String> {
    let infos = scrape_cells(port, cells)?;
    let per = |field: &str| -> Vec<u64> {
        infos.iter().map(|c| c.get(field).and_then(|v| v.parse().ok()).unwrap_or(0)).collect()
    };
    let rotations = per("segment_rotations");
    let recycled = per("segments_recycled");
    let truncated = per("segments_truncated");
    Ok(S39bSnap {
        at_s: t0.elapsed().as_secs_f64(),
        zero_fill: sum_field(&infos, "zero_fill_bytes"),
        frame_bytes: sum_field(&infos, "log_frame_bytes"),
        padding: sum_field(&infos, "log_padding_bytes"),
        host_bytes: sum_field(&infos, "accounted_host_write_bytes"),
        ckpt_bytes: sum_field(&infos, "ckpt_bytes_total"),
        recycled: recycled.iter().sum(),
        rotations: rotations.iter().sum(),
        misses: sum_field(&infos, "recycle_misses"),
        fallbacks: sum_field(&infos, "recycle_fallbacks"),
        pool_full: sum_field(&infos, "recycle_pool_full"),
        truncated: truncated.iter().sum(),
        waits_started: sum_field(&infos, "recycle_waits_started"),
        waits_satisfied: sum_field(&infos, "recycle_waits_satisfied"),
        waits_expired: sum_field(&infos, "recycle_waits_expired"),
        rotations_unzeroed: sum_field(&infos, "rotations_unzeroed"),
        inline_preallocs: sum_field(&infos, "segment_inline_preallocs"),
        prealloc_failures: sum_field(&infos, "segment_prealloc_failures"),
        rotations_min_cell: rotations.iter().copied().min().unwrap_or(0),
        truncated_min_cell: truncated.iter().copied().min().unwrap_or(0),
        deficit_max_cell: rotations
            .iter()
            .zip(&recycled)
            .map(|(r, c)| r.saturating_sub(*c))
            .max()
            .unwrap_or(0),
        device_sectors_written: device_stat.map_or(0, device_sectors_written),
    })
}

/// Sectors written on `/sys/block/<dev>/stat` (field 7) — host-to-device
/// traffic including journal and metadata writes, still blind to NAND
/// amplification (ADR-0090 A3: disclosed as such). 0 when unreadable.
fn device_sectors_written(dev: &str) -> u64 {
    std::fs::read_to_string(format!("/sys/block/{dev}/stat"))
        .ok()
        .and_then(|s| s.split_whitespace().nth(6).and_then(|v| v.parse().ok()))
        .unwrap_or(0)
}

/// One S39b leg's facts: the whole-leg client figures, the first-
/// generation snapshot (cumulative at the trigger) and the warmed deltas
/// (end − trigger), the read leg, and the crash-restart recovery time.
struct S39bLeg {
    arm: &'static str,
    ops_per_sec: f64,
    p50_us: f64,
    p99_us: f64,
    max_us: f64,
    barrier_p50_us: f64,
    barrier_p99_us: f64,
    parked: u64,
    read_ops_per_sec: f64,
    first_gen: S39bSnap,
    end: S39bSnap,
    /// `None` when the trigger never fired (the leg was too short for
    /// a truncation + rotation on every cell — the row is then invalid).
    warmed: bool,
    /// Crash-restart recovery: exactly one first boot of the fresh crashed
    /// image, after the row's drive-state idle (`--leg-idle-s`). A second
    /// boot would time an image recovery already classified and cleaned.
    recovery_s: f64,
    recover_residue_slacks: u64,
    recover_residue_stops: u64,
}

impl S39bLeg {
    fn d(&self, f: impl Fn(&S39bSnap) -> u64) -> u64 {
        f(&self.end).saturating_sub(f(&self.first_gen))
    }
    fn warmed_zero_fill_share(&self) -> f64 {
        self.d(|s| s.zero_fill) as f64 / self.d(|s| s.frame_bytes).max(1) as f64
    }
    fn first_gen_zero_fill_share(&self) -> f64 {
        self.first_gen.zero_fill as f64 / self.first_gen.frame_bytes.max(1) as f64
    }
    fn warmed_padding_pct(&self) -> f64 {
        self.d(|s| s.padding) as f64 * 100.0 / self.d(|s| s.frame_bytes).max(1) as f64
    }
    fn warmed_host_per_log(&self) -> f64 {
        self.d(|s| s.host_bytes) as f64 / self.d(|s| s.frame_bytes).max(1) as f64
    }
    fn warmed_device_per_log(&self) -> f64 {
        (self.d(|s| s.device_sectors_written) * 512) as f64
            / self.d(|s| s.frame_bytes).max(1) as f64
    }
    /// Waits ended `satisfied` over waits ended at all, warmed window
    /// (ADR-0090 A9: "waits end predominantly satisfied"); `None` when no
    /// wait ended (the wait-off arm, or recycling off).
    fn warmed_wait_satisfied_share(&self) -> Option<f64> {
        let ended = self.d(|s| s.waits_satisfied) + self.d(|s| s.waits_expired);
        (ended > 0).then(|| self.d(|s| s.waits_satisfied) as f64 / ended as f64)
    }
    fn warmed_deficit(&self) -> u64 {
        // `rotations − recycled` over the warmed window, worst cell: the
        // end snapshot's worst-cell deficit less what the first
        // generation owed by construction (its rotations were never
        // recyclable).
        self.end.deficit_max_cell.saturating_sub(self.first_gen.deficit_max_cell)
    }
}

/// Spawns the S39b row's server on `dir` (fresh unless `reuse`) with the
/// row's segment size, checkpoint floor and the arm's recycling knob, and
/// — fresh only — creates + fans the flat `always` namespace.
pub(super) fn s39b_spawn(
    flags: &Flags,
    infinityd: &str,
    cells: u16,
    dir: &str,
    arm_args: &[&str],
    reuse: bool,
) -> Result<(crate::gaterun::ServerGuard, u16), String> {
    if !reuse {
        let _ = std::fs::remove_dir_all(dir);
        std::fs::create_dir_all(dir).map_err(|e| format!("{dir}: {e}"))?;
        crate::m2rows::copy_probe_file(flags, std::path::Path::new(dir))?;
    }
    let segment_bytes = flags.u64_or("segment-bytes", S39B_DEFAULT_SEGMENT_BYTES)?;
    let ckpt_floor = flags.u64_or("ckpt-interval-bytes", segment_bytes)?;
    let mut extra: Vec<String> = vec![
        "--data-dir".into(),
        dir.to_string(),
        "--segment-bytes".into(),
        segment_bytes.to_string(),
        "--ckpt-interval-bytes".into(),
        ckpt_floor.to_string(),
    ];
    if let Some(pin) = flags.get("pin-start") {
        extra.push("--pin-start".into());
        extra.push(pin.to_string());
    }
    extra.extend(crate::m2rows::pipeline_args(flags));
    extra.extend(arm_args.iter().map(|s| (*s).to_string()));
    let extra_refs: Vec<&str> = extra.iter().map(String::as_str).collect();
    let server = spawn_infinityd(infinityd, cells, &extra_refs)?;
    let port = server.port;
    if !reuse {
        create_ns(
            port,
            &[b"INF.NS", b"CREATE", b"s39alw", b"MODE", b"durable", b"FSYNC", b"always"],
        )?;
        await_fan(port, "s39alw", cells)?;
    }
    Ok((server, port))
}

/// Crash-restart recovery time: the server is SIGKILLed (the guard's
/// drop), respawned on the same data dir, and timed to `loading:0` on
/// every cell (the recycled log's residue is what the reader meets).
fn s39b_recovery(
    flags: &Flags,
    infinityd: &str,
    cells: u16,
    dir: &str,
    arm_args: &[&str],
) -> Result<(f64, u64, u64), String> {
    let t0 = Instant::now();
    let (mut server, port) = s39b_spawn(flags, infinityd, cells, dir, arm_args, true)?;
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        if let Some(status) = server.try_exited() {
            return Err(format!("s39b: server exited during recovery ({status})"));
        }
        if let Ok(infos) = scrape_cells(port, cells)
            && sum_field(&infos, "loading") == 0
        {
            let elapsed = t0.elapsed().as_secs_f64();
            let slacks = sum_field(&infos, "recover_recycled_residue_slacks");
            let stops = sum_field(&infos, "recover_segment_residue_stops");
            return Ok((elapsed, slacks, stops));
        }
        if Instant::now() >= deadline {
            return Err("s39b: recovery never reached loading:0 within 120 s".into());
        }
        #[allow(clippy::disallowed_methods)] // bench orchestration, not cell code
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// One S39b leg: the 32-conn closed-loop `always` write leg polled every
/// second for the first-generation trigger (every cell truncated ≥ 1
/// and rotated ≥ 2 — uniform across arms, independent of recycling),
/// the end snapshot, a read leg, then the crash-restart recovery timing.
#[allow(clippy::too_many_arguments)]
fn s39b_leg(
    flags: &Flags,
    infinityd: &str,
    cells: u16,
    duration: u64,
    idle_s: u64,
    dir: &str,
    arm: &'static str,
    arm_args: &[&str],
    device_stat: Option<&str>,
) -> Result<S39bLeg, String> {
    let (server, port) = s39b_spawn(flags, infinityd, cells, dir, arm_args, false)?;
    let t0 = Instant::now();
    let spec = LoadSpec {
        port,
        conns: S35_CONNS_AC,
        pipeline: 1,
        duration: Duration::from_secs(duration),
        warmup: Duration::from_secs(2),
        set_weight: 1,
        get_weight: 0,
        keys: FILL_KEYS,
        key_prefix: "s39alw:".into(),
        value_size: 1024,
        setup: vec![vec![b"INF.NS".to_vec(), b"USE".to_vec(), b"s39alw".to_vec()]],
        ..LoadSpec::default()
    };
    let before = s39b_snap(port, cells, device_stat, t0)?;
    let load = std::thread::spawn(move || run_load(&spec));
    // The poller: one INFO scrape per second until the trigger fires.
    let mut first_gen: Option<S39bSnap> = None;
    while !load.is_finished() {
        #[allow(clippy::disallowed_methods)] // bench orchestration, not cell code
        std::thread::sleep(Duration::from_millis(1000));
        if first_gen.is_none()
            && let Ok(snap) = s39b_snap(port, cells, device_stat, t0)
            && snap.truncated_min_cell >= 1
            && snap.rotations_min_cell >= 2
        {
            first_gen = Some(snap);
        }
    }
    let report = load.join().map_err(|_| "s39b: load thread panicked".to_string())??;
    if report.errors > report.busy_retryable {
        return Err(format!(
            "s39b {arm}: {} non-BUSY errors (first: {:?})",
            report.errors - report.busy_retryable,
            report.error_samples.first()
        ));
    }
    let end = s39b_snap(port, cells, device_stat, t0)?;
    let infos = scrape_cells(port, cells)?;
    let mut p50s: Vec<f64> = infos
        .iter()
        .filter_map(|c| c.get("fua_latency_p50_us").and_then(|v| v.parse::<f64>().ok()))
        .collect();
    if p50s.is_empty() {
        return Err("s39b: INFO field fua_latency_p50_us absent — pre-S34 binary?".into());
    }
    let parked = sum_field(&infos, "log_admission_parked_total");
    let warmed = first_gen.is_some();
    let first_gen = first_gen.unwrap_or_else(|| before.clone());
    // Read leg (the ±2 % non-regression AC of every E4.7 story).
    let read = run_load(&LoadSpec {
        port,
        conns: 64,
        pipeline: 16,
        duration: Duration::from_secs(duration.min(10)),
        warmup: Duration::from_secs(1),
        set_weight: 0,
        get_weight: 1,
        keys: FILL_KEYS,
        key_prefix: "s39alw:".into(),
        value_size: 1024,
        setup: vec![vec![b"INF.NS".to_vec(), b"USE".to_vec(), b"s39alw".to_vec()]],
        ..LoadSpec::default()
    })?;
    // Crash (SIGKILL), leave the fresh image untouched during the
    // drive-state idle, then time its first and only recovery boot.
    drop(server);
    s35_idle(idle_s, &format!("s39b {arm} fresh-image recovery boot"));
    let (recovery_s, recover_residue_slacks, recover_residue_stops) =
        s39b_recovery(flags, infinityd, cells, dir, arm_args)?;
    Ok(S39bLeg {
        arm,
        ops_per_sec: report.ops_per_sec,
        p50_us: report.p50_us as f64,
        p99_us: report.p99_us as f64,
        max_us: report.max_us as f64,
        barrier_p50_us: median(&mut p50s),
        barrier_p99_us: crate::gaterun::max_field(&infos, "fua_latency_p99_us") as f64,
        parked,
        read_ops_per_sec: read.ops_per_sec,
        first_gen,
        end,
        warmed,
        recovery_s,
        recover_residue_slacks,
        recover_residue_stops,
    })
}

/// The M4.5-S39b segment-recycling row (ADR-0090 D6 as amended): the
/// S35 shape — a flat `always` namespace so zero-fill engages — run long
/// enough for ≥ 8 rotations per cell, baseline (`--no-segment-recycle`)
/// against the arm (`--segment-recycle-slots N`, row default 1), interleaved
/// per replicate (ABBA), every counter snapshotted at the end of the
/// first generation and at the end of the leg so the warmed figures are
/// **deltas**; the block device's sectors-written sampled beside the
/// accounted host bytes; a read leg and a crash-restart recovery timing
/// per leg. Gates bind on the warmed zero-fill share, the per-cell
/// recycle deficit, the padding control (S39c's term untouched), the
/// `always` p50/p99 and read ratios, and the recovery-time ratio.
pub(super) fn s39b_recycle_row(
    flags: &Flags,
    infinityd: &str,
    cells: u16,
    duration: u64,
    replicates: usize,
    data_root: &str,
    m: &mut Measurements,
) -> Result<(), String> {
    let idle_s = flags.u64_or("leg-idle-s", S35_LEG_IDLE_S)?;
    let slots = flags.u64_or("segment-recycle-slots", 1)?;
    let device_stat = flags.get("device-stat").map(str::to_string);
    let segment_bytes = flags.u64_or("segment-bytes", S39B_DEFAULT_SEGMENT_BYTES)?;
    let class = flags.str_or("barrier-class", "flush");
    if class != "fua" {
        return Err("s39b: the row needs --barrier-class fua (recycling is a Direct-class \
                    mechanism; under FLUSH nothing is pre-zeroed and nothing recycles)"
            .into());
    }
    let slots_arg = slots.to_string();
    // ADR-0090 A9: the arm's pool wait (`--recycle-wait`, the server's
    // default when absent) and which baseline the row pairs it with —
    // `recycle-off` (D6: `--no-segment-recycle`) or `wait-off` (D9: the
    // same pool bound with `--recycle-wait off`, the causal A/B for the
    // wait alone).
    let wait = flags.get("recycle-wait").map(str::to_string);
    let baseline = flags.str_or("s39b-baseline", "recycle-off");
    let mut arm_args: Vec<&str> = vec!["--segment-recycle-slots", &slots_arg];
    if let Some(wait) = wait.as_deref() {
        arm_args.extend(["--recycle-wait", wait]);
    }
    let base_args: Vec<&str> = match baseline.as_str() {
        "recycle-off" => vec!["--no-segment-recycle"],
        "wait-off" => vec!["--segment-recycle-slots", &slots_arg, "--recycle-wait", "off"],
        other => {
            return Err(format!("s39b: --s39b-baseline {other}: expected recycle-off|wait-off"));
        }
    };
    m.note(format!(
        "s39b row: {cells} cells · {replicates} replicates (ABBA) · write leg {duration} s at \
         {S35_CONNS_AC} conns · segment-bytes {segment_bytes} · ckpt floor {} · arm \
         --segment-recycle-slots {slots} --recycle-wait {} vs baseline {} · device stat {} · \
         first-generation trigger: every cell truncated ≥ 1 and rotated ≥ 2",
        flags.u64_or("ckpt-interval-bytes", segment_bytes)?,
        wait.as_deref().unwrap_or("(server default)"),
        base_args.join(" "),
        device_stat.as_deref().unwrap_or("(not sampled)")
    ));
    let mut raw = String::new();
    let mut legs: Vec<(usize, S39bLeg)> = Vec::new();
    for rep in 0..replicates {
        let order: [(&'static str, &[&str]); 2] = if rep % 2 == 0 {
            [("base", &base_args), ("arm", &arm_args)]
        } else {
            [("arm", &arm_args), ("base", &base_args)]
        };
        for (arm, args) in order {
            let dir = format!("{data_root}/s39b-{arm}-rep{rep}");
            s35_idle(idle_s, &format!("s39b {arm} rep{rep}"));
            let leg = s39b_leg(
                flags,
                infinityd,
                cells,
                duration,
                idle_s,
                &dir,
                arm,
                args,
                device_stat.as_deref(),
            )?;
            raw.push_str(&format!(
                "rep{rep} {arm} c32 ops={:.0} p50_us={:.0} p99_us={:.0} max_us={:.0} \
                 barrier_p50_us={:.0} barrier_p99_us={:.0} parked={} read_ops={:.0} \
                 rotations={} recycled={} misses={} fallbacks={} pool_full={} truncated={} \
                 waits[started={} satisfied={} expired={}] rotations_unzeroed={} \
                 inline_preallocs={} prealloc_failures={} \
                 rotations_min_cell={} trigger_at_s={:.1} warmed={} \
                 firstgen[zero_fill={} frame_bytes={} share={:.3}] \
                 warmed[zero_fill={} frame_bytes={} share={:.3} padding_pct={:.1} \
                 host_per_log={:.3} device_per_log={:.3} ckpt={} deficit={}] \
                 recovery_first_boot_s={:.3} recover_residue_slacks={} \
                 recover_residue_stops={}\n",
                leg.ops_per_sec,
                leg.p50_us,
                leg.p99_us,
                leg.max_us,
                leg.barrier_p50_us,
                leg.barrier_p99_us,
                leg.parked,
                leg.read_ops_per_sec,
                leg.end.rotations,
                leg.end.recycled,
                leg.end.misses,
                leg.end.fallbacks,
                leg.end.pool_full,
                leg.end.truncated,
                leg.end.waits_started,
                leg.end.waits_satisfied,
                leg.end.waits_expired,
                leg.end.rotations_unzeroed,
                leg.end.inline_preallocs,
                leg.end.prealloc_failures,
                leg.end.rotations_min_cell,
                leg.first_gen.at_s,
                leg.warmed,
                leg.first_gen.zero_fill,
                leg.first_gen.frame_bytes,
                leg.first_gen_zero_fill_share(),
                leg.d(|s| s.zero_fill),
                leg.d(|s| s.frame_bytes),
                leg.warmed_zero_fill_share(),
                leg.warmed_padding_pct(),
                leg.warmed_host_per_log(),
                leg.warmed_device_per_log(),
                leg.d(|s| s.ckpt_bytes),
                leg.warmed_deficit(),
                leg.recovery_s,
                leg.recover_residue_slacks,
                leg.recover_residue_stops,
            ));
            println!("  s39b rep{rep} {arm}: {}", raw.lines().last().unwrap_or(""));
            let _ = std::fs::remove_dir_all(&dir);
            legs.push((rep, leg));
        }
    }
    // Per-replicate pairs (the same rep's base and arm ran back to back).
    let pair = |rep: usize, arm: &str| legs.iter().find(|(r, l)| *r == rep && l.arm == arm);
    let mut zf_arm = Vec::new();
    let mut zf_base = Vec::new();
    let mut zf_first_arm = Vec::new();
    let mut deficit = Vec::new();
    let mut pad_delta = Vec::new();
    let mut pad_base = Vec::new();
    let mut pad_arm = Vec::new();
    let mut p50_x = Vec::new();
    let mut p99_x = Vec::new();
    let mut barrier_x = Vec::new();
    let mut ops_x = Vec::new();
    let mut read_x = Vec::new();
    let mut rec_x = Vec::new();
    let mut host_arm = Vec::new();
    let mut host_base = Vec::new();
    let mut dev_arm = Vec::new();
    let mut dev_base = Vec::new();
    let mut rot_min = Vec::new();
    let mut wait_share = Vec::new();
    let mut unzeroed_arm = Vec::new();
    let mut inline_arm = Vec::new();
    let mut nospace_arm = Vec::new();
    let mut invalid = 0usize;
    for rep in 0..replicates {
        let (Some((_, b)), Some((_, a))) = (pair(rep, "base"), pair(rep, "arm")) else { continue };
        if !a.warmed || !b.warmed {
            invalid += 1;
            continue;
        }
        zf_arm.push(a.warmed_zero_fill_share());
        zf_base.push(b.warmed_zero_fill_share());
        zf_first_arm.push(a.first_gen_zero_fill_share());
        deficit.push(a.warmed_deficit() as f64);
        pad_base.push(b.warmed_padding_pct());
        pad_arm.push(a.warmed_padding_pct());
        pad_delta.push((a.warmed_padding_pct() - b.warmed_padding_pct()).abs());
        p50_x.push(a.p50_us / b.p50_us.max(1.0));
        p99_x.push(a.p99_us / b.p99_us.max(1.0));
        barrier_x.push(a.barrier_p50_us / b.barrier_p50_us.max(1.0));
        ops_x.push(a.ops_per_sec / b.ops_per_sec.max(1.0));
        read_x.push(a.read_ops_per_sec / b.read_ops_per_sec.max(1.0));
        rec_x.push(a.recovery_s / b.recovery_s.max(1e-6));
        host_arm.push(a.warmed_host_per_log());
        host_base.push(b.warmed_host_per_log());
        dev_arm.push(a.warmed_device_per_log());
        dev_base.push(b.warmed_device_per_log());
        rot_min.push(a.end.rotations_min_cell.min(b.end.rotations_min_cell) as f64);
        if let Some(share) = a.warmed_wait_satisfied_share() {
            wait_share.push(share);
        }
        unzeroed_arm.push(a.end.rotations_unzeroed as f64);
        inline_arm.push(a.end.inline_preallocs as f64);
        nospace_arm.push(a.end.prealloc_failures as f64);
    }
    if invalid > 0 {
        m.note(format!(
            "s39b: {invalid} replicate pair(s) never reached the first-generation trigger on \
             every cell — excluded; lengthen --duration or shrink --segment-bytes"
        ));
    }
    if zf_arm.is_empty() {
        m.note("s39b: no valid replicate pair — every gate key is absent (PENDING)");
    } else {
        m.set("s39b:warmed_zero_fill_share_arm", median(&mut zf_arm));
        m.set("s39b:warmed_zero_fill_share_base", median(&mut zf_base));
        m.set("s39b:first_gen_zero_fill_share_arm", median(&mut zf_first_arm));
        m.set("s39b:recycle_deficit_per_cell", median(&mut deficit));
        m.set("s39b:padding_pct_base", median(&mut pad_base));
        m.set("s39b:padding_pct_arm", median(&mut pad_arm));
        m.set("s39b:padding_delta_pts", median(&mut pad_delta));
        m.set("s39b:always_c32_p50_x", median(&mut p50_x));
        m.set("s39b:always_c32_p99_x", median(&mut p99_x));
        m.set("s39b:barrier_p50_x", median(&mut barrier_x));
        m.set("s39b:always_c32_ops_x", median(&mut ops_x));
        m.set("s39b:read_c64p16_x", median(&mut read_x));
        m.set("s39b:recovery_time_x", median(&mut rec_x));
        m.set("s39b:host_bytes_per_log_byte_arm", median(&mut host_arm));
        m.set("s39b:host_bytes_per_log_byte_base", median(&mut host_base));
        m.set("s39b:device_bytes_per_log_byte_arm", median(&mut dev_arm));
        m.set("s39b:device_bytes_per_log_byte_base", median(&mut dev_base));
        m.set("s39b:rotations_per_cell_min", rot_min.iter().copied().fold(f64::MAX, f64::min));
        // ADR-0090 A9 (the D9 row): the wait's outcome and the three facts
        // it must not move, worst replicate (a max, never a median — one
        // un-zeroed rotation is the falsifier).
        if wait_share.is_empty() {
            m.note("s39b: no pool wait ended on the arm — the wait-satisfied share is absent");
        } else {
            m.set("s39b:wait_satisfied_share_arm", median(&mut wait_share));
        }
        let worst = |v: &[f64]| v.iter().copied().fold(0.0, f64::max);
        m.set("s39b:rotations_unzeroed_arm_max", worst(&unzeroed_arm));
        m.set("s39b:inline_preallocs_arm_max", worst(&inline_arm));
        m.set("s39b:prealloc_failures_arm_max", worst(&nospace_arm));
    }
    if device_stat.is_none() {
        m.note("s39b: --device-stat not given — device bytes per log byte read 0 (not sampled)");
    }
    m.row_open("segment-recycling");
    m.row_write_amp(
        "the warmed figures are deltas between the first-generation snapshot and the leg's \
         end (ADR-0090 A4); `host_bytes_per_log_byte` is accounted host bytes (log + zero-fill \
         + checkpoint + MANIFEST) over log frame bytes, `device_bytes_per_log_byte` the block \
         device's sectors-written × 512 over the same — journal and metadata included, NAND \
         amplification not (A3)",
    );
    m.raw_section("s39b per-leg samples", &raw);
    Ok(())
}
