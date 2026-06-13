//! M1-S15: the curated seed corpus replays green on every merge. CI-sized
//! here (reduced command quota in debug); the nightly fleet runs every line
//! at full scenario size via the CLI.

use inf_sim::{Scenario, run_scenario};

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
        let mut scenario = match name {
            "m0-smoke" => Scenario::m0_smoke(seed),
            "m1-cache" => Scenario::m1_cache(seed),
            other => panic!("corpus names unknown scenario {other}"),
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
        ran += 1;
    }
    assert!(ran >= 4, "corpus unexpectedly small ({ran} entries)");
}
