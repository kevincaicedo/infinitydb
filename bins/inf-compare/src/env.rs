//! Descriptive environment capture + tier verdict (L10 honesty).
//!
//! The authoritative reference-box gate is `inf-bench env-check`
//! (governor/EPP/thermal/dirty-tree). A passing built checker is mandatory;
//! this module also captures governor/EPP for every CPU for the report. A run is
//! `DEV-TIER` (non-citable) unless `--reference-box` is given AND the box is
//! clean. It never upgrades a dirty box silently.

use std::path::{Path, PathBuf};
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
    gather_from(
        reference_box,
        unsafe_env,
        Path::new("/sys/devices/system/cpu"),
        &[PathBuf::from("target/release/inf-bench"), PathBuf::from("target/debug/inf-bench")],
    )
}

fn gather_from(
    reference_box: bool,
    unsafe_env: bool,
    cpu_root: &Path,
    checker_paths: &[PathBuf],
) -> Env {
    let kernel = read_trim("/proc/sys/kernel/osrelease").unwrap_or_else(uname_r);
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0);
    let git_sha = run_first_line("git", &["rev-parse", "--short", "HEAD"])
        .unwrap_or_else(|| "unknown".into());
    let git_dirty = !run_stdout("git", &["status", "--porcelain"]).trim().is_empty();
    let memtier_version = tool_version("memtier_benchmark");
    let redisbench_version = tool_version("redis-benchmark");
    let (envcheck, envcheck_ok) = run_envcheck(checker_paths);

    let mut reasons = Vec::new();
    let cpus = cpu_dirs(cpu_root);
    let (governor, epp) = match cpus {
        Ok(cpus) => (
            cpu_policy(&cpus, "scaling_governor", "governor", &mut reasons),
            cpu_policy(&cpus, "energy_performance_preference", "EPP", &mut reasons),
        ),
        Err(error) => {
            reasons.push(error);
            ("unknown".into(), "unknown".into())
        }
    };
    if git_dirty {
        reasons.push("git tree is dirty".to_string());
    }
    match envcheck_ok {
        Some(true) => {}
        Some(false) => reasons.push("`inf-bench env-check` failed".into()),
        None => {
            reasons.push("`inf-bench env-check` unavailable (missing or not executable)".into())
        }
    }

    let (binding, tier) = classify(reference_box, unsafe_env, &reasons);

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

fn classify(reference_box: bool, unsafe_env: bool, reasons: &[String]) -> (bool, String) {
    let binding = reference_box && reasons.is_empty();
    let tier = if binding {
        "reference-box (binding, citation-grade)"
    } else if reference_box && unsafe_env {
        "DEV-TIER (non-citable) — `--reference-box --unsafe-env` overrode a non-clean box"
    } else {
        "DEV-TIER (non-citable, L10) — plumbing/relative numbers only"
    };
    (binding, tier.into())
}

/// Enumerate every CPU, retaining sparse IDs and failing on partial enumeration.
fn cpu_dirs(root: &Path) -> Result<Vec<PathBuf>, String> {
    let mut cpus = Vec::new();
    let entries =
        std::fs::read_dir(root).map_err(|e| format!("CPU enumeration {}: {e}", root.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("CPU enumeration {}: {e}", root.display()))?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name
            .strip_prefix("cpu")
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        {
            cpus.push(entry.path());
        }
    }
    if cpus.is_empty() {
        return Err(format!("CPU enumeration {}: no CPUs found", root.display()));
    }
    cpus.sort();
    Ok(cpus)
}

fn cpu_policy(cpus: &[PathBuf], file: &str, label: &str, reasons: &mut Vec<String>) -> String {
    let mut readings = Vec::with_capacity(cpus.len());
    for cpu in cpus {
        let path = cpu.join("cpufreq").join(file);
        let value = match std::fs::read_to_string(&path) {
            Ok(value) if !value.trim().is_empty() => value.trim().to_string(),
            result => {
                let why = result.err().map_or_else(|| "empty reading".into(), |e| e.to_string());
                reasons.push(format!("{label} unavailable at {}: {why}", path.display()));
                "unknown".into()
            }
        };
        let name = cpu.file_name().expect("enumerated CPU has a name").to_string_lossy();
        if value != "performance" {
            reasons.push(format!("{name} {label} is `{value}` (need `performance`)"));
        }
        readings.push(format!("{name}={value}"));
    }
    readings.join(", ")
}

/// Shell out to a built `inf-bench env-check`. Returns `(detail, Some(passed))`
/// when the binary is found, or `(None, None)` when it is not.
fn run_envcheck(paths: &[PathBuf]) -> (Option<String>, Option<bool>) {
    let bin = paths.iter().find(|p| p.exists());
    let Some(path) = bin else {
        return (None, None);
    };
    let Ok(out) = Command::new(path).arg("env-check").output() else {
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

fn read_trim(path: impl AsRef<Path>) -> Option<String> {
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

#[cfg(test)]
mod tests;
