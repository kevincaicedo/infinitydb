//! M1-S15: the curated seed corpus replays green on every merge. CI-sized
//! here (reduced command quota in debug); the nightly fleet runs every line
//! at full scenario size via the CLI.

use inf_sim::{
    BackfillScenario, BootStormScenario, CombinedScenario, DurableScenario, RecoveryScenario,
    Scenario, SidecarScenario, TieredScenario, run_backfill_scenario, run_boot_storm_scenario,
    run_combined_scenario, run_durable_scenario, run_ns_drop_window_scenario,
    run_recovery_scenario, run_scenario, run_sidecar_scenario, run_tiered_scenario,
};

fn parse_seed(text: &str) -> u64 {
    text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")).map_or_else(
        || text.parse().expect("decimal seed"),
        |hex| u64::from_str_radix(hex, 16).expect("hex seed"),
    )
}

// nextest-long: 70 s
#[test]
fn corpus_seeds_replay_green() {
    let text = include_str!("../seeds/corpus.txt");
    let mut ran = 0;
    for line in text.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (name, seed_text) = line.split_once(' ').expect("`<scenario> <seed>` per line");
        let seed = parse_seed(seed_text.trim());
        // Durable scenarios share the real disk/recovery runner. M2 can
        // surface a legal taxonomy refusal; M3 honest document cuts may
        // not, and its scenario turns one into a violation.
        if matches!(name, "m2-durable" | "m3-document") {
            let scenario = match name {
                "m2-durable" => DurableScenario::m2_durable(seed),
                "m3-document" => DurableScenario::m3_document(seed),
                _ => unreachable!(),
            };
            let report = run_durable_scenario(&scenario);
            assert!(
                report.ok(),
                "corpus seed {line} regressed: stalled={} violations={:?}",
                report.stalled,
                report.violations
            );
            ran += 1;
            continue;
        }
        // M2.5-S14 scenarios: their own runners + verdict shapes (the
        // combined L2/pub-sub oracles; the boot-storm ready-path oracle).
        if name == "m2-combined" {
            let report = run_combined_scenario(&CombinedScenario::m2_combined(seed));
            assert!(
                report.ok(),
                "corpus seed {line} regressed: stalled={} violations={:?}",
                report.stalled,
                report.violations
            );
            ran += 1;
            continue;
        }
        // M4-S26: the command-driven tiered node (its own runner — the
        // §8.2 command audit, flush liveness, DISKFULL, drop race). A
        // legal ADR-0018 taxonomy refusal ends the run early and ok().
        if name == "m4-tiered" {
            let report = run_tiered_scenario(&TieredScenario::m4_tiered(seed));
            assert!(
                report.ok(),
                "corpus seed {line} regressed: stalled={} violations={:?}",
                report.stalled,
                report.violations
            );
            ran += 1;
            continue;
        }
        // The tiered recovery power-cut chain, at the class the seed has
        // in the default rotation (`SeedClass::of_seed`).
        if name == "m4-recovery" {
            let report = run_recovery_scenario(&RecoveryScenario::m4_recovery(seed));
            assert!(
                report.ok(),
                "corpus seed {line} regressed: violations={:?}",
                report.violations
            );
            ran += 1;
            continue;
        }
        // ADR-0189 D5: one seed per cut class of the namespace-drop window.
        if name == "m2-ns-drop-window" {
            let report = run_ns_drop_window_scenario(seed);
            assert!(
                report.ok(),
                "corpus seed {line} ({:?}) regressed: stalled={} violations={:?}",
                report.cut,
                report.stalled,
                report.violations
            );
            ran += 1;
            continue;
        }
        if name == "boot-storm" {
            let report = run_boot_storm_scenario(&BootStormScenario::m2_boot_storm(seed));
            assert!(
                report.ok(),
                "corpus seed {line} regressed: violations={:?}",
                report.violations
            );
            ran += 1;
            continue;
        }
        // F-L17-14 (review of 2026-08-30): the M4.5 index crash scenarios
        // join the corpus — their own runners + verdict shapes.
        if name == "m45-backfill" {
            let report = run_backfill_scenario(&BackfillScenario::m45_backfill(seed));
            assert!(
                report.ok(),
                "corpus seed {line} regressed: violations={:?}",
                report.violations
            );
            ran += 1;
            continue;
        }
        if name == "m45-sidecar" {
            let report = run_sidecar_scenario(&SidecarScenario::m45_sidecar(seed));
            assert!(
                report.ok(),
                "corpus seed {line} regressed: violations={:?}",
                report.violations
            );
            ran += 1;
            continue;
        }
        let mut scenario = match name {
            "m0-smoke" => Scenario::m0_smoke(seed),
            "m0-adversarial" => Scenario::m0_adversarial(seed),
            "m0-surface" => Scenario::m0_surface(seed),
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

/// The drop race's filler leg: the path a seed whose phase-2 keys all
/// answer from RAM at phase 9 takes (one seed in thousands of the nightly's
/// sweep), forced. The race writes a filler on one cell until the filler's
/// first key reads cold, and the DROP meets that read held at the device;
/// a filler that never pushed its key out of RAM within its bound is the
/// run's VACUOUS clause.
#[test]
fn the_drop_race_filler_leg_meets_its_held_read() {
    let scenario = TieredScenario::m4_tiered(0xC0FFEE).with_drop_race_filler();
    let report = run_tiered_scenario(&scenario);
    assert!(report.ok(), "stalled={} violations={:?}", report.stalled, report.violations);
    assert!(report.drop_race_fill_batches > 0, "the leg wrote no filler batch");
    assert_eq!(
        report.drop_race_probes, report.drop_race_fill_batches,
        "the leg probes the filler's first key once a batch, and no phase-2 key"
    );
    assert_eq!(report.drop_reads_held, 1, "the DROP met the filler key's held read");
}

/// `m4-tiered` at `0x51ee07f7`, one seed of the nightly's 4,000 from
/// `0x51EE0000`. Cell 1's page of 16 cold slots enqueues them in one reactor
/// iteration, and the device takes their 6 merged reads at one entry. The
/// page still answers 16 scheduler steps after its send: the net hands the
/// command's 56 bytes to the cell in 12 seeded chunks, a step each, and the
/// reply takes 3 more. A page's reads are batched when the device takes them
/// in fewer steps than the page has cold slots, however long the command
/// took to arrive.
#[test]
#[ignore = "the SCAN batching oracle counts every scheduler step between a page's send and its \
            reply, so the steps the net takes to deliver the command read as one device round \
            trip a key; red until the oracle counts the steps in which the device took a read"]
fn a_scan_page_is_judged_by_the_steps_its_reads_took() {
    let report = run_tiered_scenario(&TieredScenario::m4_tiered(0x51ee_07f7));
    assert!(report.ok(), "stalled={} violations={:?}", report.stalled, report.violations);
}
