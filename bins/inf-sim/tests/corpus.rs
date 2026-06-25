//! M1-S15/M2-S19: the curated seed corpus replays green on every merge.
//! Node scenarios are CI-sized here (reduced command quota in debug); the
//! durability oracle uses its CI sweep config here and is scaled by explicit
//! CLI flags for the M2 10k-seed artifact.

use inf_sim::{DurabilitySweepConfig, Scenario, run_durability_sweep, run_scenario};

fn parse_seed(text: &str) -> u64 {
    text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")).map_or_else(
        || text.parse().expect("decimal seed"),
        |hex| u64::from_str_radix(hex, 16).expect("hex seed"),
    )
}

#[test]
fn corpus_seeds_replay_green() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/seeds/corpus.txt");
    let text = std::fs::read_to_string(path).expect("seed corpus exists");
    let mut ran = 0;
    for line in text.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (name, seed_text) = line.split_once(' ').expect("`<scenario> <seed>` per line");
        let seed = parse_seed(seed_text.trim());
        match name {
            "m0-smoke" | "m1-cache" => {
                let mut scenario = if name == "m0-smoke" {
                    Scenario::m0_smoke(seed)
                } else {
                    Scenario::m1_cache(seed)
                };
                if cfg!(debug_assertions) {
                    scenario.commands = scenario.commands.min(8_000);
                }
                let report = run_scenario(&scenario);
                assert!(
                    report.ok(),
                    "corpus seed {line} regressed: stalled={} violations={:?}",
                    report.stalled,
                    report.oracle_violations
                );
            }
            "m2-durability-oracle" => {
                let report = run_durability_sweep(&DurabilitySweepConfig::ci(seed));
                assert!(
                    report.ok(),
                    "corpus seed {line} regressed: violations={:?}",
                    report.violations
                );
            }
            other => panic!("corpus names unknown scenario {other}"),
        }
        ran += 1;
    }
    assert!(ran >= 7, "corpus unexpectedly small ({ran} entries)");
}
