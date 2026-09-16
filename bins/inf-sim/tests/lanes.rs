//! F-L19-03 (review 2026-08-30): every scenario the `inf-sim` binary
//! accepts runs in an automated lane, and every planted-bug canary has a
//! driver. `inf_sim::SCENARIOS` is the registry; this test binds it to
//! the CLI dispatch (`src/main.rs`), to the smoke runner
//! (`scripts/sim-smoke.sh`), to the recipe and to the PR CI job that
//! invoke the runner, and binds the workspace's `check-cfg` canary list
//! to `scripts/sim-canaries.sh`. Ten scenarios once lived only inside a
//! `just` recipe no workflow ran, and three ran nowhere at all.

use std::collections::BTreeSet;

fn read(rel: &str) -> String {
    let path = format!("{}/{rel}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

/// A scenario name: `m<digits>…-…` or `boot-storm`.
fn is_scenario_name(s: &str) -> bool {
    if s == "boot-storm" {
        return true;
    }
    let mut chars = s.chars();
    chars.next() == Some('m')
        && chars.next().is_some_and(|c| c.is_ascii_digit())
        && s.contains('-')
        && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Every `"…"` literal in `main.rs` that is a whole scenario name — the
/// dispatch arms (`"m0-smoke" =>`, `scenario_name == "m4-cold"`, the
/// `matches!` list). Messages and paths never match the shape whole.
fn dispatched_in_main() -> BTreeSet<String> {
    let src = read("src/main.rs");
    let mut names = BTreeSet::new();
    let mut rest = src.as_str();
    while let Some(open) = rest.find('"') {
        let after = &rest[open + 1..];
        let Some(close) = after.find('"') else { break };
        let literal = &after[..close];
        if is_scenario_name(literal) {
            names.insert(literal.to_string());
        }
        rest = &after[close + 1..];
    }
    names
}

/// The `--scenario` rows of the smoke runner (`rows=( "name flags…" )`).
fn smoke_rows() -> Vec<String> {
    let script = read("../../scripts/sim-smoke.sh");
    let body = script.split("rows=(").nth(1).expect("rows=( … ) in sim-smoke.sh");
    let body = body.split(')').next().expect("closing paren");
    body.lines()
        .map(str::trim)
        .filter(|l| l.starts_with('"'))
        .map(|l| l.trim_matches('"').split_whitespace().next().expect("name").to_string())
        .collect()
}

#[test]
fn the_registry_is_the_cli_dispatch() {
    let registry: BTreeSet<String> = inf_sim::SCENARIOS.iter().map(|s| (*s).to_string()).collect();
    let dispatched = dispatched_in_main();
    assert_eq!(
        registry,
        dispatched,
        "inf_sim::SCENARIOS and the `main.rs` dispatch disagree (registry-only: {:?}; \
         dispatch-only: {:?})",
        registry.difference(&dispatched).collect::<Vec<_>>(),
        dispatched.difference(&registry).collect::<Vec<_>>()
    );
    assert_eq!(registry.len(), inf_sim::SCENARIOS.len(), "a duplicate registry entry");
}

#[test]
fn every_scenario_has_a_smoke_row_and_every_row_names_a_scenario() {
    let rows = smoke_rows();
    let registry: BTreeSet<&str> = inf_sim::SCENARIOS.iter().copied().collect();
    let in_rows: BTreeSet<&str> = rows.iter().map(String::as_str).collect();
    let unrun: Vec<&&str> = registry.iter().filter(|s| !in_rows.contains(**s)).collect();
    assert!(unrun.is_empty(), "scenarios with no row in scripts/sim-smoke.sh: {unrun:?}");
    let unknown: Vec<&&str> = in_rows.iter().filter(|s| !registry.contains(**s)).collect();
    assert!(unknown.is_empty(), "sim-smoke.sh rows naming no scenario: {unknown:?}");
}

#[test]
fn the_recipe_and_the_pr_job_run_the_smoke_script() {
    let justfile = read("../../justfile");
    let recipe = justfile.split("\nsim-smoke:\n").nth(1).expect("a sim-smoke recipe");
    let recipe = recipe.split("\n\n").next().expect("recipe body");
    assert!(
        recipe.contains("./scripts/sim-smoke.sh"),
        "`just sim-smoke` no longer runs scripts/sim-smoke.sh:\n{recipe}"
    );
    let ci = read("../../.github/workflows/infinity-ci.yml");
    assert!(ci.contains("\n  sim-smoke:\n"), "a sim-smoke job in infinity-ci.yml");
    let job_end = ci[ci.find("\n  sim-smoke:\n").expect("job")..]
        .find("\n  fuzz-smoke:")
        .expect("the next job");
    let job_text = &ci[ci.find("\n  sim-smoke:\n").expect("job")..][..job_end];
    assert!(
        job_text.contains("run: ./scripts/sim-smoke.sh"),
        "the CI sim-smoke job no longer runs scripts/sim-smoke.sh:\n{job_text}"
    );
}

/// Every `--cfg inf_canary_*` the workspace admits (`Cargo.toml`
/// `check-cfg`) is driven by `scripts/sim-canaries.sh` — a planted bug
/// nobody plants proves nothing (the F-L19-03 addendum found
/// `inf_canary_foreign_segment` compiled in and never run).
#[test]
fn every_canary_cfg_has_a_driver() {
    let cargo = read("../../Cargo.toml");
    let line = cargo.lines().find(|l| l.contains("check-cfg")).expect("a check-cfg list");
    let cfgs: Vec<&str> = line
        .split("cfg(")
        .skip(1)
        .map(|s| s.split(')').next().expect("cfg name"))
        .filter(|s| s.starts_with("inf_canary_"))
        .collect();
    assert!(!cfgs.is_empty(), "no inf_canary_* cfg in {line}");
    let driver = read("../../scripts/sim-canaries.sh");
    for cfg in cfgs {
        assert!(
            driver.lines().any(|l| l.trim().starts_with(&format!("\"{cfg} "))),
            "{cfg} is admitted by check-cfg but scripts/sim-canaries.sh never plants it"
        );
    }
}
