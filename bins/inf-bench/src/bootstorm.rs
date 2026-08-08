//! `inf-bench boot-storm` (M2.5-S01): the spawn-after-heavy-writeback
//! regression harness for the ADR-0022 D7 boot wedge. Each cycle
//! reproduces the captured conditions — leave the filesystem with a dirty
//! writeback + unlink backlog (the prior gate-run leg's signature), spawn
//! a durable node on a fresh data dir, and require every cell to publish
//! RecoveryBoard-ready inside a tight bound. Pre-S01, ~1-in-10 such
//! spawns wedged one cell forever inside a blocking boot fsync; post-S01
//! the ready path is fsync-free, so a wedge here is a regression verdict.
//!
//! The gate-run spawn-retry stays in `m2rows` as a tripwire that must
//! read zero; this harness never retries — a wedge is the finding.
//!
//! Usage:
//!   inf-bench boot-storm --infinityd-bin PATH [--cycles 500] [--cells 4]
//!       [--pressure-mb 2048] [--data-root DIR] [--ready-timeout-s 10]
//!       [--pin-start N] [--artifacts-root DIR]
//!
//! `--data-root` must live on the filesystem under test (never tmpfs —
//! the wedge is journal physics; tmpfs has none).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::cli::Flags;
use crate::gaterun::{scrape_cells, spawn_infinityd, sum_field};

const KNOWN: &[&str] = &[
    "infinityd-bin",
    "cycles",
    "cells",
    "pressure-mb",
    "data-root",
    "ready-timeout-s",
    "pin-start",
    "artifacts-root",
];

pub(crate) fn cmd_boot_storm(args: &[String]) -> Result<(), String> {
    let flags = Flags::parse(args, &[], KNOWN)?;
    let bin = flags.str_or("infinityd-bin", "target/release/infinityd");
    let cycles = flags.u64_or("cycles", 500)?;
    let cells = flags.u16_or("cells", 4)?;
    let pressure_mb = flags.u64_or("pressure-mb", 2048)?;
    let ready_timeout = Duration::from_secs(flags.u64_or("ready-timeout-s", 10)?);
    let pin_start = flags.get("pin-start").map(str::to_string);
    let data_root = PathBuf::from(flags.str_or(
        "data-root",
        &format!("{}/.cache/inf-bootstorm", std::env::var("HOME").unwrap_or_else(|_| ".".into())),
    ));
    let artifacts_root = flags.str_or("artifacts-root", ".artifacts/m2.5");

    std::fs::create_dir_all(&data_root).map_err(|e| format!("{}: {e}", data_root.display()))?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_secs();
    let out_dir = PathBuf::from(&artifacts_root).join(format!("{stamp}-boot-storm"));
    std::fs::create_dir_all(&out_dir).map_err(|e| format!("{}: {e}", out_dir.display()))?;
    // Wedge captures follow the S22 stderr discipline: the caller exports
    // INF_GATERUN_STDERR_DIR (spawn_infinityd reads it) — remind them.
    if std::env::var("INF_GATERUN_STDERR_DIR").is_err() {
        eprintln!(
            "boot-storm: hint — export INF_GATERUN_STDERR_DIR={} to capture wedge stderr",
            out_dir.display()
        );
    }

    let mut ready_ms: Vec<u64> = Vec::with_capacity(cycles as usize);
    let mut wedges = 0u64;
    let mut wedge_notes: Vec<String> = Vec::new();
    // Loud fail-stop exits (`cell N failed: …` — ADR-0026 D3, e.g.
    // io_uring_setup ENOMEM under the synthetic pressure) are the named
    // Phase-H follow-up, not the silent-wedge class S01 gates on. Counted
    // and reported separately; on a quiet device both must read zero.
    let mut spawn_failstops = 0u64;
    let mut failstop_notes: Vec<String> = Vec::new();

    for cycle in 0..cycles {
        // 1. Writeback pressure: buffered writes, never fsynced — then an
        //    unlink storm of the previous cycle's tree (journal work).
        let pressure_dir = data_root.join(format!("pressure-{cycle}"));
        write_pressure(&pressure_dir, pressure_mb).map_err(|e| format!("pressure {cycle}: {e}"))?;
        if cycle > 0 {
            let _ = std::fs::remove_dir_all(data_root.join(format!("pressure-{}", cycle - 1)));
            let _ = std::fs::remove_dir_all(data_root.join(format!("data-{}", cycle - 1)));
        }

        // 2. Spawn on a fresh data dir under the pressure.
        let data_dir = data_root.join(format!("data-{cycle}"));
        let data_dir_s = data_dir.to_string_lossy().into_owned();
        let mut extra: Vec<&str> = vec!["--data-dir", &data_dir_s];
        if let Some(pin) = &pin_start {
            extra.push("--pin-start");
            extra.push(pin);
        }
        let started = Instant::now();
        let mut server = match spawn_infinityd(&bin, cells, &extra) {
            Ok(server) => server,
            Err(err) if err.contains("exited before ready") => {
                spawn_failstops += 1;
                failstop_notes.push(format!("cycle {cycle}: {err}"));
                continue;
            }
            Err(err) => {
                wedges += 1;
                wedge_notes.push(format!("cycle {cycle}: spawn/listen wedge: {err}"));
                continue;
            }
        };

        // 3. All cells ready (INFO loading == 0) inside the bound. A server
        //    that *exits* while we poll is a fail-stop (its listener bound
        //    at setup step 10, before the ring create at step 12, so it can
        //    die after accepting the readiness probe) — classified as the
        //    named Phase-H item, never as a silent wedge.
        let deadline = started + ready_timeout;
        let mut loaded = false;
        let mut exited = None;
        while Instant::now() < deadline {
            if let Some(status) = server.try_exited() {
                exited = Some(status);
                break;
            }
            match scrape_cells(server.port, cells) {
                Ok(infos) if sum_field(&infos, "loading") == 0 => {
                    loaded = true;
                    break;
                }
                _ => {
                    #[allow(clippy::disallowed_methods)] // bench orchestration, not cell code
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        }
        if loaded {
            ready_ms.push(u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX));
        } else if let Some(status) = exited {
            spawn_failstops += 1;
            failstop_notes.push(format!(
                "cycle {cycle}: server fail-stopped during setup ({status}; \
                 stderr: infinityd-{}.stderr)",
                server.port
            ));
        } else {
            wedges += 1;
            wedge_notes.push(format!(
                "cycle {cycle}: node stayed -LOADING past {}s (stderr: infinityd-{}.stderr)",
                ready_timeout.as_secs(),
                server.port
            ));
        }
        drop(server); // SIGKILL-equivalent teardown; next cycle unlinks the tree.
    }
    // Final cleanup of the last cycle's debris.
    let _ = std::fs::remove_dir_all(data_root.join(format!("pressure-{}", cycles - 1)));
    let _ = std::fs::remove_dir_all(data_root.join(format!("data-{}", cycles - 1)));

    ready_ms.sort_unstable();
    let pct = |p: f64| -> u64 {
        if ready_ms.is_empty() {
            return 0;
        }
        let idx = ((ready_ms.len() as f64 - 1.0) * p / 100.0).round() as usize;
        ready_ms[idx]
    };
    let report = format!(
        "# boot-storm (M2.5-S01)\n\n\
         - date: {stamp} (unix)\n\
         - kernel: {kernel}\n\
         - infinityd: {bin}\n\
         - cycles: {cycles} · cells: {cells} · pressure: {pressure_mb} MiB/cycle · \
           ready bound: {timeout}s · pin-start: {pin}\n\
         - data-root: {root} (must not be tmpfs)\n\n\
         | metric | value |\n|---|---|\n\
         | wedges (gate: 0) | {wedges} |\n\
         | named fail-stop exits (ADR-0026 D3 Phase-H item; informational under pressure) | {spawn_failstops} |\n\
         | retries consumed (by design) | 0 |\n\
         | time-to-all-ready p50 | {p50} ms |\n\
         | time-to-all-ready p99 | {p99} ms |\n\
         | time-to-all-ready max | {max} ms |\n\
         {notes}{failstop_section}",
        kernel = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default().trim(),
        timeout = ready_timeout.as_secs(),
        pin = pin_start.as_deref().unwrap_or("none"),
        root = data_root.display(),
        p50 = pct(50.0),
        p99 = pct(99.0),
        max = ready_ms.last().copied().unwrap_or(0),
        notes = if wedge_notes.is_empty() {
            String::new()
        } else {
            format!("\n## wedges\n\n{}\n", wedge_notes.join("\n"))
        },
        failstop_section = if failstop_notes.is_empty() {
            String::new()
        } else {
            format!("\n## named fail-stop exits\n\n{}\n", failstop_notes.join("\n"))
        },
    );
    let report_path = out_dir.join("report.md");
    std::fs::write(&report_path, &report).map_err(|e| format!("{}: {e}", report_path.display()))?;
    println!("{report}");
    println!("boot-storm: artifact {}", report_path.display());
    if wedges > 0 {
        return Err(format!("{wedges} wedge(s) — the S01 gate requires zero"));
    }
    Ok(())
}

/// Buffered, never-fsynced writes across a few files: the dirty-writeback
/// half of the captured conditions (the unlink storm is the other half).
fn write_pressure(dir: &Path, total_mb: u64) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let chunk = vec![0xA5u8; 1 << 20];
    let files = 8u64;
    let per_file_mb = total_mb.div_ceil(files);
    for f in 0..files {
        let mut file = std::fs::File::create(dir.join(format!("p{f}.bin")))?;
        for _ in 0..per_file_mb {
            file.write_all(&chunk)?;
        }
        // No sync of any kind: the pages stay dirty for the kernel to
        // write back while the node under test boots.
    }
    Ok(())
}
