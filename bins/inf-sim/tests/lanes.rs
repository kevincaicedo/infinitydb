#![allow(
    clippy::disallowed_methods,
    reason = "test: inspect source and CI files outside simulated cells (ADR-0144 D5)"
)]
//! F-L19-03 (review 2026-08-30): every scenario the `inf-sim` binary
//! accepts runs in an automated lane, and every planted-bug canary has a
//! driver. `inf_sim::SCENARIOS` is the registry; this test binds it to
//! the CLI dispatch (`src/main.rs`), to the smoke runner
//! (`scripts/sim-smoke.sh`), to the recipe and to the PR CI job that
//! invoke the runner, and binds the workspace's `check-cfg` canary list
//! to `scripts/sim-canaries.sh`. Ten scenarios once lived only inside a
//! `just` recipe no workflow ran, and three ran nowhere at all.

use std::collections::BTreeSet;

use inf_sim::CloseLeg;

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
    let registry: BTreeSet<String> =
        inf_sim::SCENARIOS.iter().map(|row| row.name.to_string()).collect();
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
    let registry: BTreeSet<&str> = inf_sim::SCENARIOS.iter().map(|row| row.name).collect();
    let in_rows: BTreeSet<&str> = rows.iter().map(String::as_str).collect();
    let unrun: Vec<&&str> = registry.iter().filter(|s| !in_rows.contains(**s)).collect();
    assert!(unrun.is_empty(), "scenarios with no row in scripts/sim-smoke.sh: {unrun:?}");
    let unknown: Vec<&&str> = in_rows.iter().filter(|s| !registry.contains(**s)).collect();
    assert!(unknown.is_empty(), "sim-smoke.sh rows naming no scenario: {unknown:?}");
}

/// The `--close-leg` value of every `m0-capacity` row of the smoke runner.
fn capacity_smoke_legs() -> BTreeSet<String> {
    let script = read("../../scripts/sim-smoke.sh");
    let body = script.split("rows=(").nth(1).expect("rows=( … ) in sim-smoke.sh");
    let body = body.split(')').next().expect("closing paren");
    body.lines()
        .map(|line| line.trim().trim_matches('"'))
        .filter(|row| row.split_whitespace().next() == Some("m0-capacity"))
        .filter_map(|row| row.split("--close-leg ").nth(1))
        .map(|rest| rest.split_whitespace().next().expect("a leg name").to_string())
        .collect()
}

/// The legs that break the one-lane rule, each as `<leg>: <why>`: a leg on
/// a smoke row and in `pinned` or on neither, and a pinned leg no test of
/// `source` runs.
fn one_lane_breaks(rows: &BTreeSet<String>, pinned: &[CloseLeg], source: &str) -> Vec<String> {
    let mut breaks = Vec::new();
    for leg in CloseLeg::ALL {
        let row = rows.contains(leg.name());
        let red = pinned.contains(&leg);
        if row == red {
            breaks.push(format!("{}: smoke row {row}, pinned red {red}", leg.name()));
        }
        if red && !source.contains(&format!("pinned_red(CloseLeg::{leg:?});")) {
            breaks.push(format!("{}: pinned red with no test that runs it", leg.name()));
        }
    }
    breaks
}

/// Every `m0-capacity` leg runs in exactly one lane: a smoke row, or
/// `CloseLeg::PINNED_RED` with the ignored test that runs it, so a leg can
/// be born neither unrun nor twice. A pinned test fails whatever its leg
/// reads, so an ignore removed cannot move a leg into a third lane.
#[test]
fn every_capacity_leg_has_a_smoke_row_or_a_pinned_red() {
    let rows = capacity_smoke_legs();
    let source = read("src/harness/capacity.rs");
    let breaks = one_lane_breaks(&rows, CloseLeg::PINNED_RED, &source);
    assert!(breaks.is_empty(), "m0-capacity legs outside one lane: {breaks:?}");
    let known: BTreeSet<&str> = CloseLeg::ALL.iter().map(|leg| leg.name()).collect();
    let unknown: Vec<&String> = rows.iter().filter(|leg| !known.contains(leg.as_str())).collect();
    assert!(unknown.is_empty(), "sim-smoke.sh rows naming no m0-capacity leg: {unknown:?}");
}

/// The one-lane rule's plants, each against this tree's rows, table and
/// source: a pinned leg given a smoke row, a pinned leg dropped from the
/// table, and a pinned leg whose test is gone each break it.
#[test]
fn the_one_lane_rule_refuses_each_plant() {
    let rows = capacity_smoke_legs();
    let source = read("src/harness/capacity.rs");
    let leg = CloseLeg::PINNED_RED[0];
    let broke =
        |breaks: Vec<String>| breaks.iter().any(|b| b.starts_with(&format!("{}: ", leg.name())));
    let mut both = rows.clone();
    both.insert(leg.name().to_string());
    assert!(broke(one_lane_breaks(&both, CloseLeg::PINNED_RED, &source)), "a leg in both lanes");
    let unpinned: Vec<CloseLeg> =
        CloseLeg::PINNED_RED.iter().copied().filter(|pinned| *pinned != leg).collect();
    assert!(broke(one_lane_breaks(&rows, &unpinned, &source)), "a leg in neither lane");
    let untested = source.replace(&format!("pinned_red(CloseLeg::{leg:?});"), "");
    assert!(
        broke(one_lane_breaks(&rows, CloseLeg::PINNED_RED, &untested)),
        "a pinned leg untested"
    );
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
    let cfgs: Vec<&str> =
        check_cfg_names(&cargo).into_iter().filter(|s| s.starts_with("inf_canary_")).collect();
    assert!(!cfgs.is_empty(), "no inf_canary_* cfg in Cargo.toml's check-cfg list");
    let driver = read("../../scripts/sim-canaries.sh");
    for cfg in cfgs {
        assert!(
            driver.lines().any(|l| l.trim().starts_with(&format!("\"{cfg} "))),
            "{cfg} is admitted by check-cfg but scripts/sim-canaries.sh never plants it"
        );
    }
}

/// The names `Cargo.toml`'s `check-cfg` array admits: one `'cfg(<name>)',`
/// a line, the name lowercase letters, digits and `_`, from the line that
/// ends `check-cfg = [` to the `]` that closes it. Any other line inside is
/// refused, so a name never hides on a line that holds another.
fn check_cfg_names(cargo: &str) -> Vec<&str> {
    let mut lines = cargo.lines().skip_while(|l| !l.trim_end().ends_with("check-cfg = ["));
    assert!(lines.next().is_some(), "no line of Cargo.toml ends `check-cfg = [`");
    let mut names = Vec::new();
    for line in lines {
        let line = line.trim();
        if line.starts_with(']') {
            assert!(!names.is_empty(), "the check-cfg list is empty");
            return names;
        }
        let name = line
            .strip_prefix("'cfg(")
            .and_then(|rest| rest.strip_suffix(")',"))
            .filter(|name| {
                !name.is_empty()
                    && name
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
            })
            .unwrap_or_else(|| panic!("a check-cfg line holds one name, `'cfg(<name>)',`: {line}"));
        names.push(name);
    }
    panic!("the check-cfg list has no closing `]`");
}

/// The first two adjacent names out of byte order, a repeat included.
fn first_out_of_order<'a>(names: &[&'a str]) -> Option<(&'a str, &'a str)> {
    names.windows(2).find(|pair| pair[0] >= pair[1]).map(|pair| (pair[0], pair[1]))
}

/// The `check-cfg` list is one name a line in byte order, so two changes
/// that each admit a cfg touch different lines and merge without a
/// conflict.
#[test]
fn the_check_cfg_list_is_one_name_a_line_in_order() {
    let cargo = read("../../Cargo.toml");
    let names = check_cfg_names(&cargo);
    if let Some((before, after)) = first_out_of_order(&names) {
        panic!("check-cfg lists {before} before {after}: each name goes in its byte-order place");
    }
}

/// The order check's plants: two names swapped, and one repeated.
#[test]
fn the_check_cfg_order_check_sees_a_swap_and_a_repeat() {
    let swapped = "x = { check-cfg = [\n    'cfg(inf_b)',\n    'cfg(inf_a)',\n] }\n";
    assert_eq!(first_out_of_order(&check_cfg_names(swapped)), Some(("inf_b", "inf_a")));
    let repeated = "x = { check-cfg = [\n    'cfg(inf_a)',\n    'cfg(inf_a)',\n] }\n";
    assert_eq!(first_out_of_order(&check_cfg_names(repeated)), Some(("inf_a", "inf_a")));
    let sorted = "x = { check-cfg = [\n    'cfg(inf_a)',\n    'cfg(inf_b)',\n] }\n";
    assert_eq!(check_cfg_names(sorted), ["inf_a", "inf_b"]);
    assert_eq!(first_out_of_order(&check_cfg_names(sorted)), None);
}

/// The reader's plant: two names on one line are refused, never read as
/// one name or as none.
#[test]
#[should_panic(expected = "a check-cfg line holds one name")]
fn two_check_cfg_names_on_one_line_are_refused() {
    check_cfg_names("x = { check-cfg = [\n    'cfg(inf_a)', 'cfg(inf_b)',\n] }\n");
}
