//! M4.5-S39d rows: recovery attribution per phase (fixed-work row).

use super::*;

// ---- M4.5-S39d: fixed-work recovery attribution (ADR-0090 A10) -------------

/// Warm-phase records per leg unless `--s39d-warm-records` says
/// otherwise: at 1 KiB values and the product's 256 MiB segments ~3 M
/// records are ≈ 3.3 GB of frames across the cells — ≥ 3 rotations per
/// cell at 4 cells, so the first generation truncates and the arm's pool
/// feeds before the checkpoint boundary (the S39b trigger, checked per
/// leg and disclosed).
const S39D_DEFAULT_WARM_RECORDS: u64 = 3_000_000;

/// Tail records applied after the checkpoint boundary (the replay work)
/// unless `--s39d-tail-records` says otherwise: ≈ 220 MB of frames across
/// the cells — inside one segment per cell, so the tail and the slack
/// behind it sit in the segment the arm recycled and the baseline
/// pre-zeroed.
const S39D_DEFAULT_TAIL_RECORDS: u64 = 200_000;

/// One cell's boot decomposed, as `INFO persistence` reports it after
/// `loading:0` (M4.5-S39d; the engine credits the loop clock between
/// consecutive recovery steps to the phase that ran — the µs sum to the
/// total within rounding).
#[derive(Clone, Copy, Debug, Default)]
struct S39dPhases {
    start_us: u64,
    ckpt_us: u64,
    ckpt_bytes: u64,
    replay_us: u64,
    replay_bytes: u64,
    replay_frames: u64,
    audit_us: u64,
    audit_bytes: u64,
    audit_valid_frames: u64,
    audit_foreign_frames: u64,
    finish_us: u64,
    stale_files: u64,
    records: u64,
    total_us: u64,
    residue_slacks: u64,
    residue_stops: u64,
}

impl S39dPhases {
    fn from_info(info: &std::collections::BTreeMap<String, String>) -> S39dPhases {
        let f = |k: &str| info.get(k).and_then(|v| v.parse().ok()).unwrap_or(0);
        S39dPhases {
            start_us: f("recover_start_us"),
            ckpt_us: f("recover_ckpt_us"),
            ckpt_bytes: f("recover_ckpt_bytes"),
            replay_us: f("recover_replay_us"),
            replay_bytes: f("recover_replay_bytes"),
            replay_frames: f("recover_replay_frames"),
            audit_us: f("recover_audit_us"),
            audit_bytes: f("recover_audit_bytes"),
            audit_valid_frames: f("recover_audit_valid_frames"),
            audit_foreign_frames: f("recover_audit_foreign_frames"),
            finish_us: f("recover_finish_us"),
            stale_files: f("recover_stale_files_removed"),
            records: f("recover_records"),
            total_us: f("recover_total_us"),
            residue_slacks: f("recover_recycled_residue_slacks"),
            residue_stops: f("recover_segment_residue_stops"),
        }
    }

    /// Durations in pipeline order.
    const fn phase_us(&self) -> [u64; 5] {
        [self.start_us, self.ckpt_us, self.replay_us, self.audit_us, self.finish_us]
    }

    /// The phase that took longest (pipeline order on ties).
    fn dominating(&self) -> &'static str {
        let mut best = 0;
        for (i, us) in self.phase_us().iter().enumerate() {
            if *us > self.phase_us()[best] {
                best = i;
            }
        }
        S39D_PHASE_NAMES[best]
    }
}

const S39D_PHASE_NAMES: [&str; 5] = ["start", "ckpt", "replay", "audit", "finish"];

/// One S39d leg: the fixed work it wrote (identical across arms by
/// construction — the counts are asserted, the bytes disclosed), the
/// warm-phase recycling facts, and the one first boot's decomposition:
/// per cell from `INFO`, the slowest cell (the boot's critical path),
/// the sum over cells (the work), the harness wall to `loading:0`, and
/// the process's device reads from `/proc/<pid>/io`.
struct S39dLeg {
    arm: &'static str,
    warm_ops_per_sec: f64,
    tail_ops_per_sec: f64,
    warm: S39bSnap,
    end: S39bSnap,
    /// Every cell truncated ≥ 1 and rotated ≥ 2 before the boundary.
    warmed: bool,
    boot_wall_s: f64,
    proc_read_bytes: u64,
    cells: Vec<S39dPhases>,
    /// `--s39d-cold-boot`: the files and bytes `inf cache-evict` synced
    /// and advised out of the page cache before the boot (`None` = the
    /// step was not asked for — a warm boot where the log is buffered).
    cold_boot: Option<(u64, u64)>,
}

impl S39dLeg {
    /// The slowest cell by engine total — recovery runs per cell in
    /// parallel, so the boot's critical path is the max, not the sum.
    fn slowest(&self) -> S39dPhases {
        self.cells.iter().copied().max_by_key(|c| c.total_us).unwrap_or_default()
    }
    fn sum(&self, f: impl Fn(&S39dPhases) -> u64) -> u64 {
        self.cells.iter().map(f).sum()
    }
    fn records_recovered(&self) -> u64 {
        self.sum(|c| c.records)
    }
}

/// `INF.CKPT WAIT` on its own long-timeout connection: the boundary
/// checkpoint streams the whole warm dataset under the device budget,
/// which at the row's size is tens of seconds.
fn s39d_checkpoint_wait(port: u16) -> Result<f64, String> {
    let t0 = Instant::now();
    let mut stream = connect("127.0.0.1", port)?;
    stream.set_read_timeout(Some(Duration::from_secs(900))).map_err(|e| format!("timeout: {e}"))?;
    let reply = request(&mut stream, &[b"INF.CKPT", b"WAIT"])?;
    if !reply.starts_with(b"+OK") {
        return Err(format!("s39d: INF.CKPT WAIT: {}", String::from_utf8_lossy(&reply)));
    }
    Ok(t0.elapsed().as_secs_f64())
}

/// Polls until `segments_truncated` holds still for `settle` (the
/// post-checkpoint truncation slices — what feeds the arm's pool — ran
/// to completion) or `bound` elapses; returns the settled snapshot.
fn s39d_settle_truncation(
    port: u16,
    cells: u16,
    device_stat: Option<&str>,
    t0: Instant,
    settle: Duration,
    bound: Duration,
) -> Result<S39bSnap, String> {
    let deadline = Instant::now() + bound;
    let mut last = s39b_snap(port, cells, device_stat, t0)?;
    let mut still_since = Instant::now();
    loop {
        #[allow(clippy::disallowed_methods)] // bench orchestration, not cell code
        std::thread::sleep(Duration::from_millis(250));
        let snap = s39b_snap(port, cells, device_stat, t0)?;
        if snap.truncated != last.truncated || snap.recycled != last.recycled {
            still_since = Instant::now();
        }
        last = snap;
        if still_since.elapsed() >= settle || Instant::now() >= deadline {
            return Ok(last);
        }
    }
}

/// `read_bytes` of `/proc/<pid>/io` — bytes the process caused to be
/// fetched from the storage layer (0 when unreadable; disclosed).
fn proc_read_bytes(pid: u32) -> u64 {
    std::fs::read_to_string(format!("/proc/{pid}/io"))
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("read_bytes:"))
                .and_then(|v| v.trim().parse().ok())
        })
        .unwrap_or(0)
}

/// Exactly one first boot of the untouched crashed image: wall time to
/// `loading:0` on every cell, the per-cell phase decomposition, and the
/// process's storage reads at that instant.
fn s39d_boot(
    flags: &Flags,
    infinityd: &str,
    cells: u16,
    dir: &str,
    arm_args: &[&str],
) -> Result<(f64, u64, Vec<S39dPhases>), String> {
    let t0 = Instant::now();
    let (mut server, port) = s39b_spawn(flags, infinityd, cells, dir, arm_args, true)?;
    let deadline = Instant::now() + Duration::from_secs(300);
    loop {
        if let Some(status) = server.try_exited() {
            return Err(format!("s39d: server exited during recovery ({status})"));
        }
        if let Ok(infos) = scrape_cells(port, cells)
            && sum_field(&infos, "loading") == 0
        {
            let wall = t0.elapsed().as_secs_f64();
            let reads = proc_read_bytes(server.pid());
            return Ok((wall, reads, infos.iter().map(S39dPhases::from_info).collect()));
        }
        if Instant::now() >= deadline {
            return Err("s39d: recovery never reached loading:0 within 300 s".into());
        }
        #[allow(clippy::disallowed_methods)] // bench orchestration, not cell code
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// One S39d leg (ADR-0090 A10): warm with exactly `warm` records (fill
/// mode — every key once, partitioned, pipelined), force and await the
/// boundary checkpoint, let truncation settle, apply exactly `tail`
/// records and wait for every ack (`always`: acked = durable), SIGKILL,
/// idle the untouched image, boot once.
#[allow(clippy::too_many_arguments)]
fn s39d_leg(
    flags: &Flags,
    infinityd: &str,
    cells: u16,
    warm: u64,
    tail: u64,
    idle_s: u64,
    dir: &str,
    arm: &'static str,
    arm_args: &[&str],
    device_stat: Option<&str>,
) -> Result<S39dLeg, String> {
    let (server, port) = s39b_spawn(flags, infinityd, cells, dir, arm_args, false)?;
    let t0 = Instant::now();
    let fill = |prefix: &str, n: u64| LoadSpec {
        port,
        conns: S35_CONNS_AC,
        pipeline: 16,
        fill: Some(n),
        keys: n,
        key_prefix: prefix.to_string(),
        value_size: 1024,
        setup: vec![vec![b"INF.NS".to_vec(), b"USE".to_vec(), b"s39alw".to_vec()]],
        ..LoadSpec::default()
    };
    let check = |report: &crate::load::LoadReport, leg: &str| -> Result<(), String> {
        if report.errors > report.busy_retryable {
            return Err(format!(
                "s39d {arm} {leg}: {} non-BUSY errors (first: {:?})",
                report.errors - report.busy_retryable,
                report.error_samples.first()
            ));
        }
        Ok(())
    };
    let warm_report = run_load(&fill("s39d:", warm))?;
    check(&warm_report, "warm")?;
    let ckpt_s = s39d_checkpoint_wait(port)?;
    let warm_snap = s39d_settle_truncation(
        port,
        cells,
        device_stat,
        t0,
        Duration::from_secs(2),
        Duration::from_secs(60),
    )?;
    let warmed = warm_snap.truncated_min_cell >= 1 && warm_snap.rotations_min_cell >= 2;
    println!(
        "  s39d {arm}: warm {warm} records at {:.0} ops/s, boundary checkpoint {ckpt_s:.1} s, \
         truncated {} recycled {} rotations_min_cell {} (warmed={warmed})",
        warm_report.ops_per_sec,
        warm_snap.truncated,
        warm_snap.recycled,
        warm_snap.rotations_min_cell
    );
    let tail_report = run_load(&fill("s39dt:", tail))?;
    check(&tail_report, "tail")?;
    let end = s39b_snap(port, cells, device_stat, t0)?;
    // Crash (SIGKILL), leave the fresh image untouched during the
    // drive-state idle, then time its first and only recovery boot.
    drop(server);
    s35_idle(idle_s, &format!("s39d {arm} fresh-image recovery boot"));
    // M4.5-S34 (campaign M3's finding): a buffered log survives SIGKILL
    // in the page cache while an `O_DIRECT` one does not, so a warm boot
    // compares RAM to the device. The cold step evicts every file of
    // the crashed image (sync + fadvise DONTNEED, `inf cache-evict`)
    // after the idle and immediately before the boot — the power-loss
    // shape on both arms.
    let cold_boot = if flags.bool("s39d-cold-boot") {
        Some(s39d_cache_evict(flags, infinityd, dir, arm)?)
    } else {
        None
    };
    let (boot_wall_s, proc_read_bytes, cells_phases) =
        s39d_boot(flags, infinityd, cells, dir, arm_args)?;
    Ok(S39dLeg {
        arm,
        warm_ops_per_sec: warm_report.ops_per_sec,
        tail_ops_per_sec: tail_report.ops_per_sec,
        warm: warm_snap,
        end,
        warmed,
        boot_wall_s,
        proc_read_bytes,
        cells: cells_phases,
        cold_boot,
    })
}

/// Runs `inf cache-evict <dir>` (the `inf` beside `--infinityd-bin`
/// unless `--inf-bin` names one) and parses its one report line:
/// `(files, bytes)`. A failure is the leg's — a boot after a partial
/// eviction must never be read as cold.
fn s39d_cache_evict(
    flags: &Flags,
    infinityd: &str,
    dir: &str,
    arm: &str,
) -> Result<(u64, u64), String> {
    let default = std::path::Path::new(infinityd)
        .parent()
        .map_or_else(|| "inf".to_string(), |p| p.join("inf").to_string_lossy().into_owned());
    let inf = flags.str_or("inf-bin", &default);
    let output = std::process::Command::new(&inf)
        .args(["cache-evict", dir])
        .output()
        .map_err(|e| format!("s39d {arm}: spawn {inf} cache-evict: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "s39d {arm}: {inf} cache-evict {dir} failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let field = |name: &str| -> Option<u64> {
        stdout
            .split_whitespace()
            .find_map(|tok| tok.strip_prefix(name).and_then(|v| v.strip_prefix('=')))
            .and_then(|v| v.parse().ok())
    };
    let (Some(files), Some(bytes)) = (field("files"), field("bytes")) else {
        return Err(format!("s39d {arm}: unparseable cache-evict report: {}", stdout.trim()));
    };
    println!("  s39d {arm}: cold boot — cache-evict {files} files, {bytes} bytes");
    Ok((files, bytes))
}

/// The M4.5-S39d fixed-work recovery row (ADR-0090 A10): baseline
/// (`--no-segment-recycle`) against the arm (the server's default —
/// one slot + the D9 pool wait, or `--segment-recycle-slots` /
/// `--recycle-wait` when given), interleaved per replicate (ABBA), both
/// arms given the same records, the same checkpoint boundary, the same
/// tail, one fresh crashed image, the same idle and exactly one boot.
/// The figure is decomposed: per-phase bytes and loop-clock time on the
/// slowest cell (the critical path) and summed over cells (the work),
/// the arm ÷ baseline ratio per phase, the dominating phase per arm,
/// the process's storage reads, and the fixed-work identities (records
/// recovered equal across arms and equal to the records written). The
/// total ratio is **diagnostic** (≤ 1.05 informational); the absolute
/// first-boot wall on the arm re-reads the S18 < 15 s gate on the
/// recycled log.
pub(super) fn s39d_recovery_row(
    flags: &Flags,
    infinityd: &str,
    cells: u16,
    replicates: usize,
    data_root: &str,
    m: &mut Measurements,
) -> Result<(), String> {
    let idle_s = flags.u64_or("leg-idle-s", S35_LEG_IDLE_S)?;
    let warm = flags.u64_or("s39d-warm-records", S39D_DEFAULT_WARM_RECORDS)?;
    let tail = flags.u64_or("s39d-tail-records", S39D_DEFAULT_TAIL_RECORDS)?;
    let device_stat = flags.get("device-stat").map(str::to_string);
    let segment_bytes = flags.u64_or("segment-bytes", S39B_DEFAULT_SEGMENT_BYTES)?;
    if flags.str_or("barrier-class", "flush") != "fua" {
        return Err("s39d: the row needs --barrier-class fua (recycling is a Direct-class \
                    mechanism; under FLUSH nothing is pre-zeroed and nothing recycles)"
            .into());
    }
    let slots = flags.get("segment-recycle-slots").map(str::to_string);
    let wait = flags.get("recycle-wait").map(str::to_string);
    let mut arm_args: Vec<&str> = Vec::new();
    if let Some(slots) = slots.as_deref() {
        arm_args.extend(["--segment-recycle-slots", slots]);
    }
    if let Some(wait) = wait.as_deref() {
        arm_args.extend(["--recycle-wait", wait]);
    }
    // `--s39d-baseline recycling-off` (ADR-0090 A10, the default) pairs
    // the recycled log against an unrecycled one. `flush-class` (M4.5-S34
    // AC, 2026-08-25) pairs the aligned FUA-class default against the
    // packed FLUSH-class log the pre-S34 path wrote — the same fixed
    // work replayed from both formats, the C38b "replay within 1 %"
    // clause on the loop clock. The server's `--barrier-class` is
    // last-wins, so the baseline's `flush` overrides the campaign's
    // `fua`; the arm stays the server default.
    let baseline = flags.str_or("s39d-baseline", "recycling-off");
    let base_args: Vec<&str> = match baseline.as_str() {
        "recycling-off" => vec!["--no-segment-recycle"],
        "flush-class" => vec!["--barrier-class", "flush"],
        other => {
            return Err(format!("--s39d-baseline is recycling-off|flush-class, got {other}"));
        }
    };
    if baseline == "flush-class" {
        m.note(
            "s39d --s39d-baseline flush-class: the baseline log is packed (no 4 KiB frame \
             alignment), so `frame_bytes_x` reads above 1.0 by the arm's padding share by \
             design — a format difference, not a fixed-work violation; the replay-phase keys \
             compare the same records from both formats",
        );
    }
    m.note(format!(
        "s39d row: {cells} cells · {replicates} replicates (ABBA) · fixed work: {warm} warm + \
         {tail} tail records × 1 KiB at {S35_CONNS_AC} conns pipeline 16 · segment-bytes \
         {segment_bytes} · ckpt floor {} · boundary = INF.CKPT WAIT after the warm fill, \
         truncation settled 2 s · SIGKILL after the tail's last ack · {idle_s} s idle · {} · \
         one boot · arm {} vs baseline {} · device stat {}",
        flags.u64_or("ckpt-interval-bytes", segment_bytes)?,
        if flags.bool("s39d-cold-boot") {
            "cold boot (every file of the image sync + fadvise DONTNEED via `inf cache-evict` \
             after the idle, both arms — the power-loss shape)"
        } else {
            "warm boot (the page cache as SIGKILL left it: a buffered log replays from RAM, \
             a direct one from the device — not a format comparison)"
        },
        if arm_args.is_empty() { "(server default)".to_string() } else { arm_args.join(" ") },
        base_args.join(" "),
        device_stat.as_deref().unwrap_or("(not sampled)")
    ));
    let mut raw = String::new();
    let mut legs: Vec<(usize, S39dLeg)> = Vec::new();
    for rep in 0..replicates {
        let order: [(&'static str, &[&str]); 2] = if rep % 2 == 0 {
            [("base", &base_args), ("arm", &arm_args)]
        } else {
            [("arm", &arm_args), ("base", &base_args)]
        };
        for (arm, args) in order {
            let dir = format!("{data_root}/s39d-{arm}-rep{rep}");
            s35_idle(idle_s, &format!("s39d {arm} rep{rep}"));
            let leg = s39d_leg(
                flags,
                infinityd,
                cells,
                warm,
                tail,
                idle_s,
                &dir,
                arm,
                args,
                device_stat.as_deref(),
            )?;
            let slow = leg.slowest();
            raw.push_str(&format!(
                "rep{rep} {arm} warm_ops={:.0} tail_ops={:.0} warmed={} \
                 warm[rotations={} recycled={} truncated={} rotations_min_cell={} \
                 waits_expired={}] end[rotations={} recycled={} truncated={} frame_bytes={} \
                 zero_fill={}] cold_boot={} boot_wall_s={:.3} proc_read_bytes={} \
                 records_recovered={} \
                 slowest_cell[total_ms={:.1} start_ms={:.1} ckpt_ms={:.1} replay_ms={:.1} \
                 audit_ms={:.1} finish_ms={:.1} dominating={}] \
                 sum_cells[total_ms={:.1} ckpt_ms={:.1} replay_ms={:.1} audit_ms={:.1} \
                 finish_ms={:.1} ckpt_bytes={} replay_bytes={} replay_frames={} audit_bytes={} \
                 audit_valid_frames={} audit_foreign_frames={} stale_files={} \
                 residue_slacks={} residue_stops={}]\n",
                leg.warm_ops_per_sec,
                leg.tail_ops_per_sec,
                leg.warmed,
                leg.warm.rotations,
                leg.warm.recycled,
                leg.warm.truncated,
                leg.warm.rotations_min_cell,
                leg.warm.waits_expired,
                leg.end.rotations,
                leg.end.recycled,
                leg.end.truncated,
                leg.end.frame_bytes,
                leg.end.zero_fill,
                leg.cold_boot.map_or("warm".to_string(), |(f, b)| format!("{f}files/{b}B")),
                leg.boot_wall_s,
                leg.proc_read_bytes,
                leg.records_recovered(),
                slow.total_us as f64 / 1e3,
                slow.start_us as f64 / 1e3,
                slow.ckpt_us as f64 / 1e3,
                slow.replay_us as f64 / 1e3,
                slow.audit_us as f64 / 1e3,
                slow.finish_us as f64 / 1e3,
                slow.dominating(),
                leg.sum(|c| c.total_us) as f64 / 1e3,
                leg.sum(|c| c.ckpt_us) as f64 / 1e3,
                leg.sum(|c| c.replay_us) as f64 / 1e3,
                leg.sum(|c| c.audit_us) as f64 / 1e3,
                leg.sum(|c| c.finish_us) as f64 / 1e3,
                leg.sum(|c| c.ckpt_bytes),
                leg.sum(|c| c.replay_bytes),
                leg.sum(|c| c.replay_frames),
                leg.sum(|c| c.audit_bytes),
                leg.sum(|c| c.audit_valid_frames),
                leg.sum(|c| c.audit_foreign_frames),
                leg.sum(|c| c.stale_files),
                leg.sum(|c| c.residue_slacks),
                leg.sum(|c| c.residue_stops),
            ));
            println!("  s39d rep{rep} {arm}: {}", raw.lines().last().unwrap_or(""));
            let _ = std::fs::remove_dir_all(&dir);
            legs.push((rep, leg));
        }
    }
    let pair = |rep: usize, arm: &str| legs.iter().find(|(r, l)| *r == rep && l.arm == arm);
    let mut total_x = Vec::new();
    let mut wall_x = Vec::new();
    let mut wall_arm = Vec::new();
    let mut wall_base = Vec::new();
    let mut phase_x: [Vec<f64>; 5] = Default::default();
    let mut audit_bytes_x = Vec::new();
    let mut reads_x = Vec::new();
    let mut frame_bytes_x = Vec::new();
    let mut records_match = Vec::new();
    let mut dominating_arm = Vec::new();
    let mut dominating_base = Vec::new();
    let mut foreign_arm = Vec::new();
    // The replay phase's rate on the slowest cell, per arm (bytes the
    // cell replayed over its loop-clock replay time) — the C38b
    // "≥ 1 GB/s per cell over the replay phase" statistic measured on
    // the instrument that row said it lacked (1-second polls), at this
    // row's shape; informational.
    let mut replay_gbps_arm = Vec::new();
    let mut replay_gbps_base = Vec::new();
    let mut invalid = 0usize;
    for rep in 0..replicates {
        let (Some((_, b)), Some((_, a))) = (pair(rep, "base"), pair(rep, "arm")) else { continue };
        if !a.warmed || !b.warmed {
            invalid += 1;
            continue;
        }
        let (sa, sb) = (a.slowest(), b.slowest());
        total_x.push(sa.total_us as f64 / sb.total_us.max(1) as f64);
        let gbps = |p: &S39dPhases| p.replay_bytes as f64 / p.replay_us.max(1) as f64 * 1e6 / 1e9;
        replay_gbps_arm.push(gbps(&sa));
        replay_gbps_base.push(gbps(&sb));
        wall_x.push(a.boot_wall_s / b.boot_wall_s.max(1e-6));
        wall_arm.push(a.boot_wall_s);
        wall_base.push(b.boot_wall_s);
        // Per phase the work ratio is over the cell sums (a phase that
        // is 0 on both arms reads 1.0, never a division by nothing).
        let sums = |l: &S39dLeg| {
            [
                l.sum(|c| c.start_us),
                l.sum(|c| c.ckpt_us),
                l.sum(|c| c.replay_us),
                l.sum(|c| c.audit_us),
                l.sum(|c| c.finish_us),
            ]
        };
        let (pa, pb) = (sums(a), sums(b));
        for i in 0..5 {
            phase_x[i].push(if pb[i] == 0 && pa[i] == 0 {
                1.0
            } else {
                pa[i] as f64 / pb[i].max(1) as f64
            });
        }
        audit_bytes_x
            .push(a.sum(|c| c.audit_bytes) as f64 / b.sum(|c| c.audit_bytes).max(1) as f64);
        reads_x.push(a.proc_read_bytes as f64 / b.proc_read_bytes.max(1) as f64);
        frame_bytes_x.push(a.end.frame_bytes as f64 / b.end.frame_bytes.max(1) as f64);
        let fixed = warm + tail;
        records_match.push(f64::from(u8::from(
            a.records_recovered() == fixed && b.records_recovered() == fixed,
        )));
        dominating_arm.push(sa.dominating());
        dominating_base.push(sb.dominating());
        foreign_arm.push(a.sum(|c| c.audit_foreign_frames) as f64);
    }
    if invalid > 0 {
        m.note(format!(
            "s39d: {invalid} replicate pair(s) never reached the first-generation trigger on \
             every cell before the boundary — excluded; raise --s39d-warm-records or shrink \
             --segment-bytes"
        ));
    }
    if total_x.is_empty() {
        m.note("s39d: no valid replicate pair — every gate key is absent (PENDING)");
    } else {
        m.set("s39d:recovery_total_x", median(&mut total_x));
        m.set("s39d:recovery_wall_x", median(&mut wall_x));
        m.set("s39d:recovery_first_boot_s_arm", median(&mut wall_arm));
        m.set("s39d:recovery_first_boot_s_base", median(&mut wall_base));
        m.set("s39d:phase_start_x", median(&mut phase_x[0]));
        m.set("s39d:phase_ckpt_x", median(&mut phase_x[1]));
        m.set("s39d:phase_replay_x", median(&mut phase_x[2]));
        m.set("s39d:phase_audit_x", median(&mut phase_x[3]));
        m.set("s39d:phase_finish_x", median(&mut phase_x[4]));
        m.set("s39d:audit_bytes_x", median(&mut audit_bytes_x));
        m.set("s39d:proc_read_bytes_x", median(&mut reads_x));
        m.set("s39d:frame_bytes_x", median(&mut frame_bytes_x));
        m.set("s39d:records_recovered_match", records_match.iter().copied().fold(1.0, f64::min));
        m.set("s39d:audit_foreign_frames_arm", median(&mut foreign_arm));
        m.set("s39d:replay_gbps_per_cell_arm", median(&mut replay_gbps_arm));
        m.set("s39d:replay_gbps_per_cell_base", median(&mut replay_gbps_base));
        let name = |v: &[&str]| -> String {
            let mut counts: std::collections::BTreeMap<&str, usize> = Default::default();
            for d in v {
                *counts.entry(d).or_default() += 1;
            }
            counts.iter().map(|(k, n)| format!("{k}×{n}")).collect::<Vec<_>>().join(" ")
        };
        m.note(format!(
            "s39d dominating phase on the slowest cell — arm: {}; baseline: {} (per replicate)",
            name(&dominating_arm),
            name(&dominating_base)
        ));
    }
    if device_stat.is_none() {
        m.note("s39d: --device-stat not given — device sectors were not sampled");
    }
    m.row_open("recovery-attribution");
    m.row_write_amp(
        "fixed-work recovery (ADR-0090 A10): `recovery_total_x` is the slowest cell's engine \
         total (first step to completion on the loop clock) arm ÷ baseline; `recovery_wall_x` \
         the harness wall from process launch to loading:0; `phase_*_x` the per-phase work \
         ratio over the cell sums; `records_recovered_match` = 1 when both arms recovered \
         exactly the records written; `frame_bytes_x` discloses the encoded-bytes parity the \
         group formation allows",
    );
    m.raw_section("s39d per-leg samples", &raw);
    Ok(())
}
