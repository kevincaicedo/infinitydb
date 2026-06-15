//! Descriptive environment capture + tier verdict (L10 honesty).
//!
//! The authoritative reference-box gate is `inf-bench env-check`
//! (governor/EPP/thermal/dirty-tree). This module shells out to it when a
//! built `inf-bench` is available and lets it *bind* the verdict; it also
//! captures the same descriptive fields directly for the report. A run is
//! `DEV-TIER` (non-citable) unless `--reference-box` is given AND the box is
//! clean. It never upgrades a dirty box silently.

use std::path::PathBuf;
use std::process::Command;

#[derive(Debug)]
pub struct Env {
    pub kernel: String,
    pub cores: usize,
    pub governor: String,
    pub epp: String,
    pub git_sha: String,
    pub git_dirty: bool,
    pub memtier_version: String,
    pub redisbench_version: String,
    /// Result of shelling out to `inf-bench env-check`, if the binary was found.
    pub envcheck: Option<String>,
    /// Human-readable tier line for the report banner.
    pub tier: String,
    /// `true` only for a clean, `--reference-box`-confirmed run.
    pub binding: bool,
    /// Why the run is not reference-grade (empty on a clean reference run).
    pub reasons: Vec<String>,
}

pub fn gather(reference_box: bool, unsafe_env: bool) -> Env {
    let kernel = read_trim("/proc/sys/kernel/osrelease").unwrap_or_else(uname_r);
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0);
    let governor = read_trim("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor")
        .unwrap_or_else(|| "unknown".into());
    let epp = read_trim("/sys/devices/system/cpu/cpu0/cpufreq/energy_performance_preference")
        .unwrap_or_else(|| "unknown".into());
    let git_sha = run_first_line("git", &["rev-parse", "--short", "HEAD"])
        .unwrap_or_else(|| "unknown".into());
    let git_dirty = !run_stdout("git", &["status", "--porcelain"]).trim().is_empty();
    let memtier_version = tool_version("memtier_benchmark");
    let redisbench_version = tool_version("redis-benchmark");
    let (envcheck, envcheck_ok) = run_envcheck();

    let mut reasons = Vec::new();
    if git_dirty {
        reasons.push("git tree is dirty".to_string());
    }
    if governor != "performance" {
        reasons.push(format!("cpu governor is `{governor}` (need `performance`)"));
    }
    if epp != "performance" && epp != "unknown" {
        reasons.push(format!("EPP is `{epp}` (need `performance`)"));
    }
    if let Some(false) = envcheck_ok {
        reasons.push("`inf-bench env-check` failed".to_string());
    }

    let binding = reference_box && reasons.is_empty();
    let tier = if binding {
        "reference-box (binding, citation-grade)".to_string()
    } else if reference_box && unsafe_env {
        "DEV-TIER (non-citable) — `--reference-box --unsafe-env` overrode a non-clean box"
            .to_string()
    } else {
        "DEV-TIER (non-citable, L10) — plumbing/relative numbers only".to_string()
    };

    Env {
        kernel,
        cores,
        governor,
        epp,
        git_sha,
        git_dirty,
        memtier_version,
        redisbench_version,
        envcheck,
        tier,
        binding,
        reasons,
    }
}

/// Shell out to a built `inf-bench env-check`. Returns `(detail, Some(passed))`
/// when the binary is found, or `(None, None)` when it is not.
fn run_envcheck() -> (Option<String>, Option<bool>) {
    let bin = ["target/release/inf-bench", "target/debug/inf-bench"]
        .into_iter()
        .map(PathBuf::from)
        .find(|p| p.exists());
    let Some(path) = bin else {
        return (None, None);
    };
    let Ok(out) = Command::new(&path).arg("env-check").output() else {
        return (Some(format!("`{} env-check` failed to run", path.display())), None);
    };
    let passed = out.status.success();
    let verdict = if passed { "PASS" } else { "FAIL" };
    let code = out.status.code().unwrap_or(-1);
    (Some(format!("{verdict} (`{} env-check` exit {code})", path.display())), Some(passed))
}

fn tool_version(program: &str) -> String {
    run_first_line(program, &["--version"]).unwrap_or_else(|| "unknown".into())
}

fn read_trim(path: &str) -> Option<String> {
    std::fs::read_to_string(path).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

fn uname_r() -> String {
    run_first_line("uname", &["-r"]).unwrap_or_else(|| "unknown".into())
}

fn run_stdout(program: &str, args: &[&str]) -> String {
    Command::new(program)
        .args(args)
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

fn run_first_line(program: &str, args: &[&str]) -> Option<String> {
    Command::new(program).args(args).output().ok().and_then(|o| {
        let bytes = if o.stdout.is_empty() { o.stderr } else { o.stdout };
        String::from_utf8_lossy(&bytes).lines().next().map(|l| l.trim().to_string())
    })
}
