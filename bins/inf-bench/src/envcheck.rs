//! `inf-bench env-check` — benchmark environment validation (M0-S03).
//!
//! Architecture: every [`Probe`] is a pure parser over an *injected reading*
//! ([`Reading`]); only the thin collectors in [`collect`] shell out or touch
//! `/sys`. That split is what the story AC means by "each unit-tested with
//! injected probes" — the tests below feed fake readings through the real
//! probe structs and never touch the host.
//!
//! Linux probes (governor, EPP, thermal throttle) are written here but can
//! only be exercised against real `/sys` files on the Linux reference box —
//! tracked as Linux-validation-pending in the crate README.

use crate::cli::Flags;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeResult {
    Pass,
    Fail(String),
    Skipped(String),
}

/// A reading handed to a probe: captured data, or the reason it could not be
/// captured (wrong OS, file not exposed, command missing). `Unavailable`
/// readings produce `Skipped`, never `Fail` — absence of evidence is not a
/// failed environment, it is a named gap in the report.
#[derive(Debug, Clone)]
pub enum Reading<T> {
    Value(T),
    Unavailable(String),
}

pub trait Probe {
    fn name(&self) -> &'static str;
    fn check(&self) -> ProbeResult;
}

// ---------------------------------------------------------------------------
// Probes (pure parsers over injected readings)
// ---------------------------------------------------------------------------

/// Dirty git tree ⇒ the binary under test is not the committed code (L10).
pub struct GitDirtyTree {
    pub porcelain: Reading<String>,
}

impl Probe for GitDirtyTree {
    fn name(&self) -> &'static str {
        "git-dirty-tree"
    }
    fn check(&self) -> ProbeResult {
        match &self.porcelain {
            Reading::Unavailable(why) => ProbeResult::Skipped(why.clone()),
            Reading::Value(s) => parse_git_porcelain(s),
        }
    }
}

pub fn parse_git_porcelain(porcelain: &str) -> ProbeResult {
    let entries = porcelain.lines().filter(|l| !l.trim().is_empty()).count();
    if entries == 0 {
        ProbeResult::Pass
    } else {
        ProbeResult::Fail(format!("dirty git tree: {entries} uncommitted entries"))
    }
}

/// Linux: every cpufreq `scaling_governor` must be `performance`.
pub struct CpufreqGovernor {
    /// `(cpu_name, governor)` per CPU, e.g. `("cpu0", "performance")`.
    pub readings: Reading<Vec<(String, String)>>,
}

impl Probe for CpufreqGovernor {
    fn name(&self) -> &'static str {
        "cpufreq-governor"
    }
    fn check(&self) -> ProbeResult {
        match &self.readings {
            Reading::Unavailable(why) => ProbeResult::Skipped(why.clone()),
            Reading::Value(r) => parse_expect_all(r, "performance", "scaling_governor"),
        }
    }
}

/// Linux: every cpufreq `energy_performance_preference` must be `performance`.
pub struct CpufreqEpp {
    pub readings: Reading<Vec<(String, String)>>,
}

impl Probe for CpufreqEpp {
    fn name(&self) -> &'static str {
        "cpufreq-epp"
    }
    fn check(&self) -> ProbeResult {
        match &self.readings {
            Reading::Unavailable(why) => ProbeResult::Skipped(why.clone()),
            Reading::Value(r) => {
                parse_expect_all(r, "performance", "energy_performance_preference")
            }
        }
    }
}

/// Shared check: every `(cpu, value)` reading must equal `expected`.
pub fn parse_expect_all(readings: &[(String, String)], expected: &str, what: &str) -> ProbeResult {
    if readings.is_empty() {
        return ProbeResult::Skipped(format!("{what} not exposed (vm/container?)"));
    }
    let bad: Vec<String> = readings
        .iter()
        .filter(|(_, v)| v.trim() != expected)
        .map(|(cpu, v)| format!("{cpu}={}", v.trim()))
        .collect();
    if bad.is_empty() {
        ProbeResult::Pass
    } else {
        let shown = bad.iter().take(4).cloned().collect::<Vec<_>>().join(", ");
        let more =
            if bad.len() > 4 { format!(" (+{} more)", bad.len() - 4) } else { String::new() };
        ProbeResult::Fail(format!("{what} != {expected} on {} cpus: {shown}{more}", bad.len()))
    }
}

/// Linux: thermal-throttle event counters must be zero.
pub struct ThermalThrottle {
    /// `(counter_name, count)` e.g. `("cpu0/core_throttle_count", 0)`.
    pub readings: Reading<Vec<(String, u64)>>,
}

impl Probe for ThermalThrottle {
    fn name(&self) -> &'static str {
        "thermal-throttle"
    }
    fn check(&self) -> ProbeResult {
        match &self.readings {
            Reading::Unavailable(why) => ProbeResult::Skipped(why.clone()),
            Reading::Value(r) => parse_throttle_counts(r),
        }
    }
}

pub fn parse_throttle_counts(readings: &[(String, u64)]) -> ProbeResult {
    if readings.is_empty() {
        return ProbeResult::Skipped("thermal_throttle counters not exposed".to_string());
    }
    let hot: Vec<String> =
        readings.iter().filter(|(_, n)| *n > 0).map(|(c, n)| format!("{c}={n}")).collect();
    if hot.is_empty() {
        ProbeResult::Pass
    } else {
        let shown = hot.iter().take(4).cloned().collect::<Vec<_>>().join(", ");
        ProbeResult::Fail(format!("thermal throttling occurred: {shown}"))
    }
}

/// macOS: must be on AC power (`pmset -g batt`).
pub struct PowerSource {
    pub pmset_batt: Reading<String>,
}

impl Probe for PowerSource {
    fn name(&self) -> &'static str {
        "power-source"
    }
    fn check(&self) -> ProbeResult {
        match &self.pmset_batt {
            Reading::Unavailable(why) => ProbeResult::Skipped(why.clone()),
            Reading::Value(s) => parse_pmset_batt(s),
        }
    }
}

pub fn parse_pmset_batt(output: &str) -> ProbeResult {
    if output.contains("Battery Power") {
        ProbeResult::Fail("running on battery power (connect AC)".to_string())
    } else if output.contains("AC Power") {
        ProbeResult::Pass
    } else {
        ProbeResult::Skipped("pmset output did not name a power source".to_string())
    }
}

/// macOS: Low Power Mode must be off (`pmset -g` → `lowpowermode 1`).
pub struct LowPowerMode {
    pub pmset_settings: Reading<String>,
}

impl Probe for LowPowerMode {
    fn name(&self) -> &'static str {
        "low-power-mode"
    }
    fn check(&self) -> ProbeResult {
        match &self.pmset_settings {
            Reading::Unavailable(why) => ProbeResult::Skipped(why.clone()),
            Reading::Value(s) => parse_pmset_lowpower(s),
        }
    }
}

pub fn parse_pmset_lowpower(output: &str) -> ProbeResult {
    for line in output.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("lowpowermode") else { continue };
        return match rest.trim() {
            "1" => ProbeResult::Fail("Low Power Mode is enabled".to_string()),
            "0" => ProbeResult::Pass,
            other => ProbeResult::Skipped(format!("unrecognized lowpowermode value `{other}`")),
        };
    }
    ProbeResult::Skipped("lowpowermode not reported by pmset".to_string())
}

// ---------------------------------------------------------------------------
// Collectors (the only code that touches the host)
// ---------------------------------------------------------------------------

pub mod collect {
    use super::Reading;
    use std::fs;
    use std::path::Path;
    use std::process::Command;

    pub fn run_capture(cmd: &str, args: &[&str]) -> Reading<String> {
        match Command::new(cmd).args(args).output() {
            Ok(out) if out.status.success() => {
                Reading::Value(String::from_utf8_lossy(&out.stdout).into_owned())
            }
            Ok(out) => Reading::Unavailable(format!("`{cmd}` exited with {}", out.status)),
            Err(e) => Reading::Unavailable(format!("`{cmd}` failed to spawn: {e}")),
        }
    }

    pub fn git_porcelain() -> Reading<String> {
        run_capture("git", &["status", "--porcelain"])
    }

    fn linux_only<T>() -> Option<Reading<T>> {
        if cfg!(target_os = "linux") {
            None
        } else {
            Some(Reading::Unavailable(format!(
                "linux-only probe (this host: {})",
                std::env::consts::OS
            )))
        }
    }

    fn macos_only<T>() -> Option<Reading<T>> {
        if cfg!(target_os = "macos") {
            None
        } else {
            Some(Reading::Unavailable(format!(
                "macos-only probe (this host: {})",
                std::env::consts::OS
            )))
        }
    }

    const CPU_BASE: &str = "/sys/devices/system/cpu";

    fn is_cpu_dir(name: &str) -> bool {
        name.strip_prefix("cpu")
            .is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
    }

    fn per_cpu_dirs() -> Result<Vec<String>, String> {
        let entries = fs::read_dir(CPU_BASE).map_err(|e| format!("{CPU_BASE}: {e}"))?;
        let mut cpus: Vec<String> = entries
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| is_cpu_dir(n))
            .collect();
        cpus.sort();
        Ok(cpus)
    }

    /// Read `<CPU_BASE>/cpuN/<rel>` for every CPU that exposes it.
    fn read_per_cpu(rel: &str) -> Reading<Vec<(String, String)>> {
        let cpus = match per_cpu_dirs() {
            Ok(c) => c,
            Err(why) => return Reading::Unavailable(why),
        };
        let mut out = Vec::new();
        for cpu in cpus {
            let path = Path::new(CPU_BASE).join(&cpu).join(rel);
            if let Ok(v) = fs::read_to_string(&path) {
                out.push((cpu, v.trim().to_string()));
            }
        }
        Reading::Value(out) // empty ⇒ the probe reports Skipped("not exposed")
    }

    pub fn cpufreq_governors() -> Reading<Vec<(String, String)>> {
        linux_only().unwrap_or_else(|| read_per_cpu("cpufreq/scaling_governor"))
    }

    pub fn cpufreq_epp() -> Reading<Vec<(String, String)>> {
        linux_only().unwrap_or_else(|| read_per_cpu("cpufreq/energy_performance_preference"))
    }

    pub fn thermal_throttle_counts() -> Reading<Vec<(String, u64)>> {
        if let Some(skip) = linux_only() {
            return skip;
        }
        let cpus = match per_cpu_dirs() {
            Ok(c) => c,
            Err(why) => return Reading::Unavailable(why),
        };
        let mut out = Vec::new();
        for cpu in cpus {
            for counter in ["core_throttle_count", "package_throttle_count"] {
                let path = Path::new(CPU_BASE).join(&cpu).join("thermal_throttle").join(counter);
                if let Ok(v) = fs::read_to_string(&path)
                    && let Ok(n) = v.trim().parse::<u64>()
                {
                    out.push((format!("{cpu}/{counter}"), n));
                }
            }
        }
        Reading::Value(out)
    }

    pub fn pmset_batt() -> Reading<String> {
        macos_only().unwrap_or_else(|| run_capture("pmset", &["-g", "batt"]))
    }

    pub fn pmset_settings() -> Reading<String> {
        macos_only().unwrap_or_else(|| run_capture("pmset", &["-g"]))
    }
}

// ---------------------------------------------------------------------------
// Runner
// ---------------------------------------------------------------------------

pub struct ProbeOutcome {
    pub name: &'static str,
    pub result: ProbeResult,
}

/// Run every probe against live host readings.
pub fn run_system_probes() -> Vec<ProbeOutcome> {
    let probes: Vec<Box<dyn Probe>> = vec![
        Box::new(GitDirtyTree { porcelain: collect::git_porcelain() }),
        Box::new(CpufreqGovernor { readings: collect::cpufreq_governors() }),
        Box::new(CpufreqEpp { readings: collect::cpufreq_epp() }),
        Box::new(ThermalThrottle { readings: collect::thermal_throttle_counts() }),
        Box::new(PowerSource { pmset_batt: collect::pmset_batt() }),
        Box::new(LowPowerMode { pmset_settings: collect::pmset_settings() }),
    ];
    probes.iter().map(|p| ProbeOutcome { name: p.name(), result: p.check() }).collect()
}

pub struct EnvVerdict {
    /// Names of failing probes, after `--allow-dirty` exclusions.
    pub failures: Vec<&'static str>,
    /// The dirty-tree probe failed but was excused by `--allow-dirty`.
    pub dirty_overridden: bool,
}

impl EnvVerdict {
    pub fn ok(&self) -> bool {
        self.failures.is_empty()
    }
}

/// Apply the dev-run policy: `--allow-dirty` excuses only the dirty-tree
/// probe (the run still prints a DIRTY banner and is not citation-grade).
pub fn evaluate(outcomes: &[ProbeOutcome], allow_dirty: bool) -> EnvVerdict {
    let mut failures = Vec::new();
    let mut dirty_overridden = false;
    for o in outcomes {
        if let ProbeResult::Fail(_) = o.result {
            if o.name == "git-dirty-tree" && allow_dirty {
                dirty_overridden = true;
            } else {
                failures.push(o.name);
            }
        }
    }
    EnvVerdict { failures, dirty_overridden }
}

pub fn print_table(outcomes: &[ProbeOutcome]) {
    println!("  {:<20} RESULT", "PROBE");
    for o in outcomes {
        let (tag, detail) = match &o.result {
            ProbeResult::Pass => ("PASS", String::new()),
            ProbeResult::Fail(why) => ("FAIL", format!("  {why}")),
            ProbeResult::Skipped(why) => ("SKIP", format!("  {why}")),
        };
        println!("  {:<20} {tag}{detail}", o.name);
    }
}

pub fn cmd_env_check(args: &[String]) -> Result<(), String> {
    let flags = Flags::parse(args, &["allow-dirty"], &["allow-dirty"])?;
    let allow_dirty = flags.bool("allow-dirty");

    println!("inf-bench env-check");
    let outcomes = run_system_probes();
    print_table(&outcomes);

    let verdict = evaluate(&outcomes, allow_dirty);
    if verdict.dirty_overridden {
        println!("\n  *** DIRTY TREE — dev run only, output is not citation-grade ***");
    }
    if verdict.ok() {
        println!("\nenv-check: OK");
        Ok(())
    } else {
        Err(format!("env-check failed: {}", verdict.failures.join(", ")))
    }
}

// ---------------------------------------------------------------------------
// Tests — fake readings injected through the real probe structs (M0-S03 AC)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn cpus(values: &[&str]) -> Vec<(String, String)> {
        values.iter().enumerate().map(|(i, v)| (format!("cpu{i}"), v.to_string())).collect()
    }

    #[test]
    fn clean_tree_passes_dirty_tree_fails() {
        let clean = GitDirtyTree { porcelain: Reading::Value(String::new()) };
        assert_eq!(clean.check(), ProbeResult::Pass);

        let dirty =
            GitDirtyTree { porcelain: Reading::Value(" M src/main.rs\n?? new.rs\n".into()) };
        let ProbeResult::Fail(why) = dirty.check() else { panic!("expected Fail") };
        assert!(why.contains("2 uncommitted"), "{why}");
    }

    #[test]
    fn git_unavailable_is_skipped_not_failed() {
        let p = GitDirtyTree { porcelain: Reading::Unavailable("`git` failed to spawn".into()) };
        assert!(matches!(p.check(), ProbeResult::Skipped(_)));
    }

    #[test]
    fn performance_governor_passes() {
        let p = CpufreqGovernor { readings: Reading::Value(cpus(&["performance", "performance"])) };
        assert_eq!(p.check(), ProbeResult::Pass);
    }

    #[test]
    fn powersave_governor_fails_with_named_cpu() {
        let p = CpufreqGovernor {
            readings: Reading::Value(cpus(&["performance", "powersave", "schedutil"])),
        };
        let ProbeResult::Fail(why) = p.check() else { panic!("expected Fail") };
        assert!(why.contains("cpu1=powersave"), "{why}");
        assert!(why.contains("cpu2=schedutil"), "{why}");
    }

    #[test]
    fn missing_cpufreq_skips() {
        let p = CpufreqGovernor { readings: Reading::Value(vec![]) };
        assert!(matches!(p.check(), ProbeResult::Skipped(_)));
        let p = CpufreqGovernor { readings: Reading::Unavailable("linux-only probe".into()) };
        assert!(matches!(p.check(), ProbeResult::Skipped(_)));
    }

    #[test]
    fn epp_balance_performance_fails() {
        let p = CpufreqEpp { readings: Reading::Value(cpus(&["balance_performance"])) };
        let ProbeResult::Fail(why) = p.check() else { panic!("expected Fail") };
        assert!(why.contains("energy_performance_preference"), "{why}");

        let ok = CpufreqEpp { readings: Reading::Value(cpus(&["performance"])) };
        assert_eq!(ok.check(), ProbeResult::Pass);
    }

    #[test]
    fn throttle_counts_zero_pass_nonzero_fail() {
        let ok = ThermalThrottle {
            readings: Reading::Value(vec![("cpu0/core_throttle_count".into(), 0)]),
        };
        assert_eq!(ok.check(), ProbeResult::Pass);

        let hot = ThermalThrottle {
            readings: Reading::Value(vec![
                ("cpu0/core_throttle_count".into(), 0),
                ("cpu1/package_throttle_count".into(), 7),
            ]),
        };
        let ProbeResult::Fail(why) = hot.check() else { panic!("expected Fail") };
        assert!(why.contains("cpu1/package_throttle_count=7"), "{why}");
    }

    #[test]
    fn battery_power_fails_ac_passes() {
        let batt = PowerSource {
            pmset_batt: Reading::Value(
                "Now drawing from 'Battery Power'\n -InternalBattery-0".into(),
            ),
        };
        assert!(matches!(batt.check(), ProbeResult::Fail(_)));

        let ac = PowerSource { pmset_batt: Reading::Value("Now drawing from 'AC Power'\n".into()) };
        assert_eq!(ac.check(), ProbeResult::Pass);

        let odd = PowerSource { pmset_batt: Reading::Value("garbage".into()) };
        assert!(matches!(odd.check(), ProbeResult::Skipped(_)));
    }

    #[test]
    fn low_power_mode_states() {
        let on = LowPowerMode {
            pmset_settings: Reading::Value("Active Profiles:\n lowpowermode        1\n".into()),
        };
        assert!(matches!(on.check(), ProbeResult::Fail(_)));

        let off = LowPowerMode {
            pmset_settings: Reading::Value(" lowpowermode        0\n displaysleep 10\n".into()),
        };
        assert_eq!(off.check(), ProbeResult::Pass);

        let absent = LowPowerMode { pmset_settings: Reading::Value("displaysleep 10\n".into()) };
        assert!(matches!(absent.check(), ProbeResult::Skipped(_)));
    }

    #[test]
    fn allow_dirty_excuses_only_the_dirty_tree() {
        let outcomes = vec![
            ProbeOutcome {
                name: "git-dirty-tree",
                result: ProbeResult::Fail("dirty git tree: 1 uncommitted entries".into()),
            },
            ProbeOutcome {
                name: "cpufreq-governor",
                result: ProbeResult::Fail("scaling_governor != performance on 1 cpus".into()),
            },
            ProbeOutcome { name: "power-source", result: ProbeResult::Pass },
        ];

        let strict = evaluate(&outcomes, false);
        assert_eq!(strict.failures, vec!["git-dirty-tree", "cpufreq-governor"]);
        assert!(!strict.dirty_overridden);

        let dev = evaluate(&outcomes, true);
        assert_eq!(dev.failures, vec!["cpufreq-governor"]);
        assert!(dev.dirty_overridden);
    }
}
