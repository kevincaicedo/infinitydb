//! `inf-sim` CLI (M0-S20): run a deterministic scenario, optionally twice,
//! comparing event traces byte-for-byte.
//!
//! ```text
//! inf-sim --scenario m0-smoke --seed 0xC0FFEE --verify-determinism
//! ```
#![forbid(unsafe_code)]
#![allow(
    clippy::disallowed_methods,
    reason = "sim-harness: scenario files and reports outside simulated cells (ADR-0144 D5)"
)]
// ADR-0144 D1: a production `match` names every variant of its enum.
#![cfg_attr(
    not(test),
    deny(clippy::wildcard_enum_match_arm, clippy::match_wildcard_for_single_variants)
)]

use inf_sim::driver::{self, Exit, Fold, Shard, Sweep, Verify};
use inf_sim::net::Plant;
use inf_sim::{
    CombinedScenario, DurableScenario, EpochMarksLeg, ExtentWindowLeg, LedgerWedgeLeg, Scenario,
    TierBudgetLeg, run_combined_scenario, run_durable_scenario, run_scenario,
};
use inf_sim::{Leg, LegValue, RecoveryScenario, ScenarioRow, SeedClass, SpecVariant, Sweeps};

use std::num::NonZeroU64;
use std::path::PathBuf;

fn parse_seed(text: &str) -> Result<u64, String> {
    let text = text.trim();
    if let Some(hex) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        u64::from_str_radix(hex, 16).map_err(|e| format!("seed: {e}"))
    } else {
        text.parse().map_err(|e| format!("seed: {e}"))
    }
}

/// The census's closest boot, `files of bound`, or that no boot sealed a
/// file.
fn closest_boot(closest: Option<(u64, u64)>) -> String {
    closest.map_or_else(
        || "none sealed a file".to_owned(),
        |(files, bound)| format!("{files} of {bound}"),
    )
}

/// ADR-0107 (review of 2026-08-30, F-L16-01): a simulator built without
/// its `dst` feature carries no fault registry and no collision oracle —
/// every armed point would fire `false` and every forced-collision row
/// would hash two distinct keys, so a "clean" run would prove nothing.
/// Refuse to run instead of reporting a lie. Exit 2 is the harness's
/// usage-error code (`run-sweep.sh` treats it as a missing simulator).
fn require_dst_build() {
    if inf_foundation::fault::COMPILED_IN && inf_foundation::COLLISION_ORACLE {
        return;
    }
    eprintln!(
        "inf-sim: this binary was built without the `dst` feature (fault points: {}, collision \
         oracle: {}) — build it with `cargo build -p inf-sim --features dst` (ADR-0107); a plain \
         workspace build carries neither so that `infinityd` never does",
        inf_foundation::fault::COMPILED_IN,
        inf_foundation::COLLISION_ORACLE
    );
    std::process::exit(2);
}

fn main() {
    require_dst_build();
    let mut scenario_name = "m0-smoke".to_string();
    let mut seed = 0xC0FFEEu64;
    let mut verify = false;
    let mut plant = Plant::None;
    let mut overrides: Vec<(String, u64)> = Vec::new();
    let mut trace_out: Option<String> = None;

    let mut sweep: Option<NonZeroU64> = None;
    let mut shard: Option<Shard> = None;
    let mut out_dir: Option<String> = None;
    let mut replay_canary = false;
    // F-L14-01 (batch 20): force the m2 lift regime on any seed — the
    // arm on a seed known to lift (the sweep discloses which).
    let mut lift_regime = false;
    // ADR-0174 D1: force the replay-above-window seed class on any seed of
    // m4-recovery and m4-tiered — the tail replay must demote; a run in it
    // that never exceeded a window is VACUOUS.
    let mut replay_above_window = false;
    // ADR-0174 D2 rule 6: force m4-recovery's spec-variant class on any
    // seed — every boot recovers at a window below its ring, with records
    // up to the inline maximum; a run that placed no pad is VACUOUS.
    let mut spec_variant: Option<SpecVariant> = None;
    let mut ops_override: Option<u64> = None;
    // A legged scenario's leg, named on every run so a sweep is one shape;
    // the registry's leg column says which flag is whose.
    let mut legs: Vec<LegValue> = Vec::new();
    // TM3's leg of m2-fill-tick and m2-group-hold: the loops at spin 0.
    let mut park_at_once = false;

    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut take = |name: &str| it.next().ok_or_else(|| format!("{name} needs a value"));
        let result: Result<(), String> = (|| {
            if let Some(leg) = Leg::of_flag(&flag) {
                legs.push(leg.parse(&take(leg.flag())?)?);
                return Ok(());
            }
            match flag.as_str() {
                "--scenario" => scenario_name = take("--scenario")?,
                "--seed" => seed = parse_seed(&take("--seed")?)?,
                "--verify-determinism" => verify = true,
                "--plant" => {
                    plant = match take("--plant")?.as_str() {
                        "lost-wakeup" => Plant::LostWakeup,
                        "fsync-lies" => Plant::FsyncLies,
                        "accept-error" => Plant::AcceptError,
                        "accept-broken" => Plant::AcceptBroken,
                        "tier-read-eio" => Plant::TierReadEio,
                        "tier-write-eio" => Plant::TierWriteEio,
                        "stop-kill" => Plant::StopKill,
                        other => return Err(format!("unknown plant {other}")),
                    }
                }
                "--cells" | "--connections" | "--commands" | "--key-space"
                | "--wheel-nodes-max" => {
                    let value = take(&flag)?.parse().map_err(|e| format!("{flag}: {e}"))?;
                    overrides.push((flag.clone(), value));
                }
                "--trace-out" => trace_out = Some(take("--trace-out")?),
                "--sweep" => sweep = Some(driver::parse_seed_count(&take("--sweep")?)?),
                "--shard" => shard = Some(Shard::parse(&take("--shard")?)?),
                "--out" => out_dir = Some(take("--out")?),
                "--replay-canary" => replay_canary = true,
                "--lift-regime" => lift_regime = true,
                "--replay-above-window" => replay_above_window = true,
                "--park-at-once" => park_at_once = true,
                "--spec-variant" => {
                    spec_variant = Some(match take("--spec-variant")?.as_str() {
                        "ring-top" => SpecVariant::RingTop,
                        "page" => SpecVariant::Page,
                        other => return Err(format!("unknown spec variant {other}")),
                    });
                }
                // m4-cold: total op count (the AC's 10⁶ run sets it; the
                // smoke default is lighter).
                "--ops" => {
                    ops_override = Some(take("--ops")?.parse().map_err(|e| format!("--ops: {e}"))?);
                }
                "--help" | "-h" => {
                    println!(
                        "inf-sim --scenario <name> (one of: {}) \
                         [--seed N|0xN] [--verify-determinism] \
                         [--plant lost-wakeup|fsync-lies|accept-error|accept-broken|\
                         tier-read-eio|tier-write-eio|stop-kill] \
                         [--replay-canary] [--lift-regime] [--replay-above-window] \
                         [--spec-variant ring-top|page] {} [--park-at-once] \
                         [--cells N] [--connections N] [--commands N] [--wheel-nodes-max N] \
                         [--trace-out FILE] \
                         [--sweep N [--shard I/K] [--out DIR]] [--list-scenarios] \
                         [--list-sweeps]",
                        scenario_names().join("|"),
                        leg_flags_help()
                    );
                    std::process::exit(0);
                }
                "--list-scenarios" => {
                    for name in scenario_names() {
                        println!("{name}");
                    }
                    std::process::exit(0);
                }
                "--list-sweeps" => {
                    list_sweeps();
                    std::process::exit(0);
                }
                other => return Err(format!("unknown flag {other}")),
            }
            Ok(())
        })();
        if let Err(e) = result {
            eprintln!("inf-sim: {e}");
            std::process::exit(2);
        }
    }
    // Every scenario is a registry row. A legged one names its leg, and only
    // it takes that leg's flag: one check over the registry's leg column.
    let Some(row) = ScenarioRow::named(&scenario_name) else {
        eprintln!(
            "inf-sim: unknown scenario {scenario_name} (have: {})",
            scenario_names().join(", ")
        );
        std::process::exit(2);
    };
    let leg = row.leg_of(&legs).unwrap_or_else(|refusal| {
        eprintln!("inf-sim: {refusal}");
        std::process::exit(2);
    });
    // The accept-broken plant is the accept-retry oracle's leg, m0-smoke's
    // alone: refused on any other scenario before a runner starts.
    if plant.is_accept_retry_leg() && scenario_name != "m0-smoke" {
        eprintln!("inf-sim: --plant accept-broken is m0-smoke's");
        std::process::exit(2);
    }
    // The park-at-once leg is the wake oracle's, m2-fill-tick's and
    // m2-group-hold's alone: refused here, before any scenario dispatches,
    // since a leg that dispatches early would run with the flag ignored.
    if park_at_once && !matches!(scenario_name.as_str(), "m2-fill-tick" | "m2-group-hold") {
        eprintln!("inf-sim: --park-at-once is m2-fill-tick's and m2-group-hold's");
        std::process::exit(2);
    }
    let determinism = if verify { Verify::Twin } else { Verify::Off };
    let sweep = sweep_of(row, verify, seed, sweep, shard, out_dir).unwrap_or_else(|refusal| {
        eprintln!("inf-sim: {refusal}");
        std::process::exit(2);
    });
    // The two flags each force one of m4-recovery's seed classes, which a
    // seed runs one of: together they are refused, never one dropped.
    let forced_class = match (replay_above_window, spec_variant) {
        (true, Some(_)) => {
            eprintln!(
                "inf-sim: --replay-above-window and --spec-variant force two seed classes; give \
                 one"
            );
            std::process::exit(2);
        }
        (true, None) => Some(SeedClass::ReplayAboveWindow),
        (false, variant) => variant.map(SeedClass::SpecVariant),
    };

    // m4-extent-window's leg arrives as its type from the leg check.
    if let ("m4-extent-window", Some(LegValue::ExtentWindow(leg))) = (scenario_name.as_str(), leg) {
        std::process::exit(run_extent_window(seed, leg, determinism).code());
    }

    // The M2-S19 durable scenario has its own runner (power cuts, the
    // durability oracle, the sweep mode).
    if let Some(kind) = DurableKind::resolve(&scenario_name, leg) {
        run_durable(
            &scenario_name,
            kind,
            seed,
            plant,
            replay_canary,
            lift_regime,
            park_at_once,
            determinism,
            sweep.as_ref(),
        );
        return;
    }
    // The M2.5-S14 combined scenario (durable + memory + pub/sub + expiry
    // on one node; its own runner — power cuts, the composed oracles, the
    // sweep mode). `--plant` does not apply (its planted bugs are source
    // mutations, per the S14 demonstrations).
    if scenario_name == "m2-combined" {
        run_m2_combined(seed, determinism, sweep.as_ref());
        return;
    }
    // The M4-S04 steel-thread scenario (tiered lifecycle + cold reads
    // through suspension + the S06 crash/replay content oracle).
    if scenario_name == "m4-steel" {
        let run_one = |seed| inf_sim::run_steel_scenario(&inf_sim::SteelScenario::m4_steel(seed));
        let exit = driver::run_single(seed, determinism, run_one, |report| {
            println!(
                "inf-sim: scenario m4-steel seed {seed:#x}: {} cold reads, {} promotions, \
                 hash {:#018x}",
                report.cold_reads, report.promotions, report.trace_hash
            );
            Ok(())
        });
        std::process::exit(exit.code());
    }
    // The M4.5-S05 backfill scenario (crash at every backfill phase;
    // ready only with oracle-verified contents — ADR-0077).
    if scenario_name == "m45-backfill" {
        let run_one =
            |seed| inf_sim::run_backfill_scenario(&inf_sim::BackfillScenario::m45_backfill(seed));
        let exit = driver::run_single(seed, determinism, run_one, |report| {
            println!(
                "inf-sim: scenario m45-backfill seed {seed:#x}: {} boots, cuts {:?}, \
                 {} ready checks, {} refused bindings, {} raced mutations, {} steps, \
                 hash {:#018x}",
                report.boots,
                report.cuts,
                report.ready_checks,
                report.refused_bindings,
                report.raced_mutations,
                report.scheduler_steps,
                report.trace_hash
            );
            print_census(report.cap_rows_checked, report.cap_rows_crossed);
            Ok(())
        });
        std::process::exit(exit.code());
    }
    // The M4.5-S06 sidecar scenario (checkpoint under mutation storm,
    // cut, boot with sidecar load + tail catch-up ⇒ oracle green;
    // mid-write cut ⇒ rebuild — ADR-0078).
    if scenario_name == "m45-sidecar" {
        let run_one =
            |seed| inf_sim::run_sidecar_scenario(&inf_sim::SidecarScenario::m45_sidecar(seed));
        let exit = driver::run_single(seed, determinism, run_one, |report| {
            println!(
                "inf-sim: scenario m45-sidecar seed {seed:#x}: {} boots, cuts {:?}, \
                 {} loaded, {} storm-during-stream, {} ready checks, {} refused bindings, \
                 {} degraded-tail rebuilds, {} steps, hash {:#018x}",
                report.boots,
                report.cuts,
                report.loaded,
                report.storm_during_stream,
                report.ready_checks,
                report.refused_bindings,
                report.degraded_rebuilds,
                report.scheduler_steps,
                report.trace_hash
            );
            print_census(report.cap_rows_checked, report.cap_rows_crossed);
            Ok(())
        });
        std::process::exit(exit.code());
    }
    // The M4-S21 disk-budget admission scenario (typed DISKFULL, refusal
    // purity, the compaction reserve, automatic recovery — ADR-0063).
    if scenario_name == "m4-diskfull" {
        let run_one =
            |seed| inf_sim::run_diskfull_scenario(&inf_sim::DiskfullScenario::m4_diskfull(seed));
        let exit = driver::run_single(seed, determinism, run_one, |report| {
            println!(
                "inf-sim: scenario m4-diskfull seed {seed:#x}: {} refusals, {} reopens, \
                 {} B relocated, {} files retired, peak disk {} B, {} keys verified, hash {:#018x}",
                report.refusals,
                report.reopens,
                report.relocated_bytes,
                report.retired_files,
                report.peak_disk_used,
                report.keys_verified,
                report.trace_hash
            );
            Ok(())
        });
        std::process::exit(exit.code());
    }
    // The M4-S07 throttled-device backpressure scenario (budget bound,
    // typed stall timeouts, deadlock freedom — ADR-0053).
    if scenario_name == "m4-pressure" {
        let run_one =
            |seed| inf_sim::run_pressure_scenario(&inf_sim::PressureScenario::m4_pressure(seed));
        let exit = driver::run_single(seed, determinism, run_one, |report| {
            println!(
                "inf-sim: scenario m4-pressure seed {seed:#x}: {} stalls (p50 {} µs, p99 {} µs), \
                 {} timeouts, peak committed {} B, hash {:#018x}",
                report.stalls,
                report.stall_p50_ns / 1_000,
                report.stall_p99_ns / 1_000,
                report.stall_timeouts,
                report.peak_committed_bytes,
                report.trace_hash
            );
            Ok(())
        });
        std::process::exit(exit.code());
    }
    // The M4-S08 cold-read storm (placement, relocation races, chunked
    // staging, cancellation, pin-deferred unlinks).
    if scenario_name == "m4-recovery" {
        let run_one = |seed: u64| {
            let mut scenario = RecoveryScenario::m4_recovery(seed);
            if let Some(class) = forced_class {
                scenario = scenario.with_class(class);
            }
            inf_sim::run_recovery_scenario(&scenario)
        };
        if let Some(sweep) = &sweep {
            std::process::exit(
                driver::run_sweep("m4-recovery", sweep, RecoveryFold::default(), run_one).code(),
            );
        }
        let exit = driver::run_single(seed, determinism, run_one, |report| {
            println!(
                "inf-sim: m4-recovery seed {seed:#x}: {} lives, {} refs, {} images, {} tail \
                 records, {} cut-before-publish, {} flush-lag, {} keys audited, {} live-set \
                 entries, {} relocations, {} retired, {} unlinked, {} left-to-boot-gc, \
                 {} blobs, {} orphans-planted, {} blob-reclaims, shadow {} opened / {} open at a \
                 cut / {} re-formed / {} same-key / {} collision, {} collide-ops, {} \
                 settled-at-boot, \
                 {} drain-checks, {} multi-ticket winners / {} DELs, {} twin-origin rows / {} \
                 origins covered, {} held rows / {} re-formed / {} not restored, {} lives above \
                 the \
                 window (largest unit {} windows), boots {} demoting / {} fitting checked, boot \
                 replay {} demote steps / {} tier bytes / {} settle reads / {} same-key / {} \
                 deletes verified / {} markers skipped, {} writer parks ({} past a walk, {} held \
                 released), {} boot files censused, two-crash rows {} opened / {} settled / {} \
                 refs removed by a marker ({} skipped: unit past the raised window), spec \
                 variant {:?}: {} pads placed, {} long records written ({} written short), boot \
                 files censused {} (capacity {} / gap {} / shutdown {} seals), {} page pads \
                 crossed, against a summed bound of {} (closest boot: {}), trace {:#x}",
                report.lives,
                report.refs_emitted,
                report.images_emitted,
                report.tail_records,
                report.cut_before_publish,
                report.flush_lag_lives,
                report.keys_audited,
                report.live_entries_emitted,
                report.relocations,
                report.files_retired,
                report.files_unlinked,
                report.unlinks_left_to_boot_gc,
                report.blobs_written,
                report.blob_orphans_planted,
                report.blob_extents_reclaimed,
                report.shadow_opened,
                report.shadow_open_at_cut,
                report.shadow_reformed,
                report.shadow_same_key,
                report.shadow_collision,
                report.shadow_collide_ops,
                report.shadow_settled_at_boot,
                report.shadow_drain_checks,
                report.shadow_multi_ticket_winners,
                report.shadow_multi_ticket_dels,
                report.shadow_twin_origin_rows,
                report.shadow_twin_origins_covered,
                report.shadow_held_rows,
                report.shadow_held_reformed,
                report.shadow_held_not_restored,
                report.replay_above_window_lives,
                report.replay_unit_windows_max,
                report.demoting_boots,
                report.fitting_boots_checked,
                report.boot_replay.demote_steps,
                report.boot_replay.tier_bytes,
                report.boot_replay.settle_reads,
                report.boot_replay.settled_same_key,
                report.boot_replay.deletes_verified,
                report.boot_replay.markers_skipped,
                report.writer_parks,
                report.writes_parked_past_a_walk,
                report.held_released_by_park,
                report.boot_files_censused,
                report.two_crash_rows_opened,
                report.two_crash_rows_settled,
                report.two_crash_markers_removed,
                report.two_crash_unit_past_window,
                report.spec_variant,
                report.boot_replay.pads_placed,
                report.long_records_written,
                report.long_records_shortened,
                report.seal_census.files,
                report.seal_census.capacity_seals,
                report.seal_census.gap_seals,
                report.seal_census.shutdown_seals,
                report.seal_census.page_pads,
                report.seal_census.bound,
                closest_boot(report.seal_census.closest),
                report.trace_hash
            );
            Ok(())
        });
        std::process::exit(exit.code());
    }

    // The M4-S26 command-driven tiered node (RESP over the sim net
    // against the wired plane: cut → recover → audit → re-pressure →
    // DISKFULL clamp → drop race). Sweep mode mirrors m4-recovery.
    if scenario_name == "m4-tiered" {
        let run_one = |seed: u64| {
            let mut scenario = inf_sim::TieredScenario::m4_tiered(seed);
            // F-L04-02: the flag forces the seed-cadence arm on.
            scenario.tier_read_fault |= plant == Plant::TierReadEio;
            if replay_above_window {
                scenario = scenario.with_replay_above_window();
            }
            inf_sim::run_tiered_scenario(&scenario)
        };
        if let Some(sweep) = &sweep {
            std::process::exit(
                driver::run_sweep("m4-tiered", sweep, TieredFold::default(), run_one).code(),
            );
        }
        // The forced arm is a positive control: it must fire on the seed a
        // single run names. The check is part of the run, so the twin judges
        // it too; a sweep holds the arm to its shard-scope check instead.
        let forced = |seed: u64| {
            let mut report = run_one(seed);
            if plant == Plant::TierReadEio && report.tier_read_faults_fired == 0 {
                report.violations.push(format!(
                    "TIER-READ FAULT ARM VACUOUS seed {seed:#x}: --plant tier-read-eio forced \
                     the arm and the audit consumed no fault (pick a seed whose flush confirmed \
                     before the cut)"
                ));
            }
            report
        };
        let exit = driver::run_single(seed, determinism, forced, |report| {
            println!(
                "inf-sim: m4-tiered seed {seed:#x}: {} commands, {} steps, {} keys audited, {} \
                 required ops, {} allowed-lost, {} B flushed pre-cut, {} B flushed final, {} cold \
                 resolves, {} blob sets, {} DISKFULL refusals (reopened: {}), drop-race {} values \
                 / \
                 {} typed-other, post-drop reboot {} (MANIFEST residue {}), cut inside DROP after \
                 {} \
                 steps: {:?}, refused-boot {}, shadow arm {} ({} tickets, {} open at the cut, \
                 {} same-key / {} collision, {} stale, {} fallbacks; phase 6b cold resolves {}; \
                 phase 6c {} pairs: {} tickets, {} collision verdicts, {} ticketed fallbacks, {} \
                 DBSIZE drains, {} SCAN twins), blob-key race {} replans, dir-open fault arm {} \
                 (fired {}), tier-read EIO arm {} (fired {}, {} typed replies), ckpt downgrades {} \
                 / bound splits {}, SCAN batching oracle on {} cold pages ({} cold intents), \
                 replay \
                 unit above the window on {} cell(s) (acked record bytes per cell {:?}), boot \
                 tier replay {:?} (demote steps, bytes, barriers, files sealed, settle reads, \
                 deletes verified, step charge max), {} aged deletes acked, trace {} bytes, hash \
                 {:#018x}",
                report.commands_done,
                report.scheduler_steps,
                report.audited_keys,
                report.required_ops,
                report.allowed_lost_ops,
                report.flushed_pre_cut_bytes,
                report.flushed_final_bytes,
                report.cold_resolves,
                report.blob_sets,
                report.diskfull_refusals,
                report.diskfull_reopened,
                report.drop_replies_value,
                report.drop_replies_other,
                report.drop_reboot_ok,
                report.drop_reboot_manifest_residue,
                report.drop_cut_steps,
                report.drop_cut_outcome,
                report.refused_boot,
                report.shadow_arm,
                report.shadow_created,
                report.shadow_pending_at_cut,
                report.shadow_resolved_same_key,
                report.shadow_resolved_collision,
                report.shadow_stale,
                report.shadow_fallbacks,
                report.phase6b_cold_resolves,
                report.collide_pairs,
                report.collide_tickets,
                report.collide_verdicts,
                report.collide_ticketed_fallbacks,
                report.collide_dbsize_drains,
                report.collide_scan_twins,
                report.race_replans,
                report.dir_open_fault_arm,
                report.dir_open_faults_fired,
                report.tier_read_fault_arm,
                report.tier_read_faults_fired,
                report.tier_read_error_replies,
                report.ckpt_downgrades,
                report.ckpt_bound_splits,
                report.scan_cold_pages,
                report.scan_cold_reads,
                report.replay_above_window_cells,
                report.acked_record_bytes_per_cell,
                report.boot_tier,
                report.aged_deletes_acked,
                report.trace.len(),
                report.trace_hash
            );
            println!(
                "inf-sim: sim_seconds={:.6} published=0 delivered=0 tier_read_fault_arm={} \
                 tier_read_faults_fired={} tier_read_error_replies={} scan_cold_pages={} \
                 scan_cold_reads={}",
                report.sim_seconds,
                report.tier_read_fault_arm,
                report.tier_read_faults_fired,
                report.tier_read_error_replies,
                report.scan_cold_pages,
                report.scan_cold_reads
            );
            print_census(report.cap_rows_checked, report.cap_rows_crossed);
            Ok(())
        });
        std::process::exit(exit.code());
    }

    // ADR-0108 (review of 2026-08-30, the batch-8 residual): concurrent
    // namespace DDL — a CREATE and a DROP of one name from two cells with
    // crossing fans, a CREATE whose peer leg is refused, then a cut and
    // the audit that every cell agrees with META.
    if scenario_name == "m2-ns-ddl-race" {
        let run_one = |seed: u64| inf_sim::run_ns_ddl_race_scenario(seed);
        if let Some(sweep) = &sweep {
            std::process::exit(
                driver::run_sweep("m2-ns-ddl-race", sweep, NsDdlFold::default(), run_one).code(),
            );
        }
        let exit = driver::run_single(seed, determinism, run_one, |report| {
            println!(
                "inf-sim: scenario m2-ns-ddl-race seed {seed:#x}: {} commands, {} steps, DROP \
                 after \
                 {} steps ({}, origin frozen {} steps), 'x' served on {} cells before the cut / {} \
                 after, refused leg {} left {} cells serving 'y', {} untombstoned unknown-ns \
                 skips, {} \
                 released keys found, trace {} bytes, hash {:#018x}",
                report.commands_done,
                report.scheduler_steps,
                report.drop_delay,
                if report.drop_found { "found" } else { "not found" },
                report.freeze_steps,
                report.served_before_cut,
                report.served_after_cut,
                report.refused_leg,
                report.partial_served,
                report.skipped_unknown_ns,
                report.released_keys_found,
                report.trace.len(),
                report.trace_hash
            );
            println!("inf-sim: sim_seconds={:.6} published=0 delivered=0", report.sim_seconds);
            print_census(report.cap_rows_checked, report.cap_rows_crossed);
            Ok(())
        });
        std::process::exit(exit.code());
    }

    // ADR-0189 D5: the namespace-drop window — the stale fold at the cap,
    // lagging exports inside a DROP's and a rollback's window, and the cut
    // class the seed picks, each with its reboot audit.
    if scenario_name == "m2-ns-drop-window" {
        let run_one = |seed: u64| inf_sim::run_ns_drop_window_scenario(seed);
        if let Some(sweep) = &sweep {
            std::process::exit(
                driver::run_sweep("m2-ns-drop-window", sweep, NsDropFold::default(), run_one)
                    .code(),
            );
        }
        let exit = driver::run_single(seed, determinism, run_one, |report| {
            println!(
                "inf-sim: scenario m2-ns-drop-window seed {seed:#x}: cut {:?}, {} commands, {} \
                 steps, \
                 stale fold {} epoch(s), {} lagging exports, {} held steps, checkpoint in the wait \
                 {}, {} acked keys served, {} untombstoned unknown-ns skips, trace {} bytes, hash \
                 {:#018x}",
                report.cut,
                report.commands_done,
                report.scheduler_steps,
                report.stale_fold_epochs,
                report.lagging_exports,
                report.held_steps,
                report.checkpoint_in_wait,
                report.keys_served,
                report.skipped_unknown_ns,
                report.trace.len(),
                report.trace_hash
            );
            println!("inf-sim: sim_seconds={:.6} published=0 delivered=0", report.sim_seconds);
            print_census(report.cap_rows_checked, report.cap_rows_crossed);
            Ok(())
        });
        std::process::exit(exit.code());
    }

    // ADR-0103 (review of 2026-08-30, C14): the namespace-creation window
    // — a held catalog swap, probes attempting `USE`/`SET` inside it, a
    // cut, and the audit that an acked write never vanishes and no tail
    // record is skipped for a namespace nothing explains. Sweep mode
    // mirrors m4-cold's shape.
    if scenario_name == "m2-ns-create-window" {
        let run_one = |seed: u64| inf_sim::run_ns_create_window_scenario(seed);
        if let Some(sweep) = &sweep {
            std::process::exit(
                driver::run_sweep("m2-ns-create-window", sweep, NsCreateFold::default(), run_one)
                    .code(),
            );
        }
        let exit = driver::run_single(seed, determinism, run_one, |report| {
            println!(
                "inf-sim: scenario m2-ns-create-window seed {seed:#x}: {} commands, {} steps, swap \
                 held {} steps, {} USE attempts ({} refused, {} writes acked in the window), {} \
                 untombstoned unknown-ns skips, {} released keys found, trace {} bytes, hash \
                 {:#018x}",
                report.commands_done,
                report.scheduler_steps,
                report.held_steps,
                report.use_attempts,
                report.use_refused,
                report.acked_in_window,
                report.skipped_unknown_ns,
                report.released_keys_found,
                report.trace.len(),
                report.trace_hash
            );
            println!("inf-sim: sim_seconds={:.6} published=0 delivered=0", report.sim_seconds);
            print_census(report.cap_rows_checked, report.cap_rows_crossed);
            Ok(())
        });
        std::process::exit(exit.code());
    }
    if scenario_name == "m4-cold" {
        let run_one = |seed| {
            let mut scenario = inf_sim::ColdStormScenario::m4_cold(seed);
            if let Some(ops) = ops_override {
                scenario.ops = ops;
            }
            inf_sim::run_cold_storm_scenario(&scenario)
        };
        let exit = driver::run_single(seed, determinism, run_one, |report| {
            println!(
                "inf-sim: scenario m4-cold seed {seed:#x}: {} gets ({} cold, {} chunked, {} \
             restarts, {} cancelled [{} early / {} late]), {} merged waiters (queue hw {}), \
             {} unlinks ({} deferrals), hash {:#018x}",
                report.gets,
                report.cold_served,
                report.chunked_reads,
                report.restarts,
                report.cancelled,
                report.cancelled_early,
                report.cancelled_late,
                report.merged_waiters,
                report.queue_high_water,
                report.unlinks,
                report.unlink_deferrals,
                report.trace_hash
            );
            Ok(())
        });
        std::process::exit(exit.code());
    }
    // The M2.5-S01 boot-storm scenario (wedge-class oracle).
    if scenario_name == "boot-storm" {
        let run_one = |seed| {
            inf_sim::run_boot_storm_scenario(&inf_sim::BootStormScenario::m2_boot_storm(seed))
        };
        let exit = driver::run_single(seed, determinism, run_one, |report| {
            println!(
                "inf-sim: scenario boot-storm seed {seed:#x}: {} boots, ready-steps max {}, \
                 hash {:#018x}",
                report.boots, report.ready_steps_max, report.trace_hash
            );
            print_census(report.cap_rows_checked, report.cap_rows_crossed);
            Ok(())
        });
        std::process::exit(exit.code());
    }
    let mut scenario = match scenario_name.as_str() {
        "m0-smoke" => Scenario::m0_smoke(seed),
        "m0-adversarial" => Scenario::m0_adversarial(seed),
        "m0-surface" => Scenario::m0_surface(seed),
        "m0-fabric-fairness" => Scenario::m0_fabric_fairness(seed),
        "m0-admission" => Scenario::m0_admission(seed),
        "m1-cache" => Scenario::m1_cache(seed),
        other => {
            eprintln!("inf-sim: unknown scenario {other} (have: {})", scenario_names().join(", "));
            std::process::exit(2);
        }
    };
    scenario.plant = plant;
    for (flag, value) in overrides {
        match flag.as_str() {
            "--cells" => scenario.cells = value as u16,
            "--connections" => scenario.connections = value as usize,
            "--commands" => scenario.commands = value,
            "--key-space" => scenario.key_space = value,
            "--wheel-nodes-max" => {
                let nodes = usize::try_from(value).unwrap_or(usize::MAX);
                match inf_store::WheelNodesMax::new(nodes) {
                    Ok(max) => scenario.wheel_nodes_max = Some(max),
                    Err(e) => {
                        eprintln!("inf-sim: --wheel-nodes-max {} is over the width bound", e.nodes);
                        std::process::exit(2);
                    }
                }
            }
            _ => unreachable!(),
        }
    }

    // The overrides are the scenario's; the twin runs the same scenario.
    let run_one = |_seed| run_scenario(&scenario);
    let exit = driver::run_single(seed, determinism, run_one, |report| {
        println!(
            "inf-sim: scenario {scenario_name} seed {seed:#x}: {} commands, {} apply events, \
             {} steps, trace {} bytes, hash {:#018x}",
            report.commands_done,
            report.events,
            report.scheduler_steps,
            report.trace.len(),
            report.trace_hash
        );
        // Machine-readable line for the nightly fleet (sim-seconds budget sum).
        println!(
            "inf-sim: sim_seconds={:.6} published={} delivered={} audits={} flushes={} \
             scan_walks={} replays_skipped={} plant_fired={} accept_resumes={} \
             fabric_skip_streak_max={} refused_clients={} idle_closed={} idle_survived={} \
             shadow_checked={} shadow_unmodeled={:?}",
            report.sim_seconds,
            report.published,
            report.delivered,
            report.audits,
            report.flushes,
            report.scan_walks,
            report.replays_skipped,
            report.plant_fired,
            report.accept_resumes,
            report.fabric_skip_streak_max,
            report.refused_clients,
            report.idle_closed,
            report.idle_survived,
            report.shadow_checked,
            report.shadow_unmodeled
        );
        print_census(report.cap_rows_checked, report.cap_rows_crossed);
        if let Some(accept_retry) = &report.accept_retry {
            print_accept_retry(accept_retry);
        }
        if let Some(path) = &trace_out
            && let Err(e) = std::fs::write(path, &report.trace)
        {
            return Err(format!("--trace-out {path}: {e}"));
        }
        if report.stalled {
            eprintln!("inf-sim: STALL — no progress for the detector window (seed {seed:#x})");
        }
        Ok(())
    });
    std::process::exit(exit.code());
}

/// The sweep arguments as one checked type, or the refusal. `--shard` and
/// `--out` need `--sweep`; `--sweep` is refused on a row outside the
/// registry's sweep column, and the message names the column;
/// `--verify-determinism` is a single run's, since
/// a sweep runs every seed twice and a flag beside it would teach that its
/// absence meant once.
fn sweep_of(
    row: &ScenarioRow,
    verify: bool,
    base_seed: u64,
    seeds: Option<NonZeroU64>,
    shard: Option<Shard>,
    out: Option<String>,
) -> Result<Option<Sweep>, String> {
    let Some(seeds) = seeds else {
        return match (shard, out) {
            (Some(_), _) => Err("--shard needs --sweep".to_owned()),
            (None, Some(_)) => Err("--out needs --sweep".to_owned()),
            (None, None) => Ok(None),
        };
    };
    if verify {
        return Err(
            "--verify-determinism is a single run's: a sweep runs every seed twice".to_owned()
        );
    }
    match row.sweeps {
        Sweeps::Yes => {
            let shard = shard.unwrap_or(Shard::WHOLE);
            Ok(Some(Sweep::new(base_seed, seeds, shard, out.map(PathBuf::from))))
        }
        Sweeps::No => Err(format!(
            "--sweep is not {}'s: the scenarios that sweep are {}",
            row.name,
            sweep_rows().map(|row| row.name).collect::<Vec<_>>().join(", ")
        )),
    }
}

/// Every registry name, in the registry's order.
fn scenario_names() -> Vec<&'static str> {
    inf_sim::SCENARIOS.iter().map(|row| row.name).collect()
}

/// The leg flags and their names, as the help text lists them.
fn leg_flags_help() -> String {
    let flags: Vec<String> =
        Leg::ALL.iter().map(|leg| format!("[{} {}]", leg.flag(), leg.names().join("|"))).collect();
    flags.join(" ")
}

/// The registry's sweep column.
fn sweep_rows() -> impl Iterator<Item = &'static ScenarioRow> {
    inf_sim::SCENARIOS.iter().filter(|row| row.sweeps == Sweeps::Yes)
}

/// `--list-sweeps`: one line per sweep-capable row, its name and, for a
/// legged row, its leg flag and first leg: the arguments a one-seed sweep
/// of the row needs.
fn list_sweeps() {
    for row in sweep_rows() {
        match row.leg {
            Some(leg) => println!("{} {} {}", row.name, leg.flag(), leg.names()[0]),
            None => println!("{}", row.name),
        }
    }
}

/// The durable runner's scenarios, each with its leg where it has one: the
/// leg arrives as its type, so no constructor asks whether it was given.
#[derive(Clone, Copy)]
enum DurableKind {
    Plain(fn(u64) -> DurableScenario),
    LedgerWedge(LedgerWedgeLeg),
    TierBudget(TierBudgetLeg),
    EpochMarks(EpochMarksLeg),
}

impl DurableKind {
    /// The durable scenario `name` and `leg` name, or `None` when the
    /// durable runner runs no such scenario.
    fn resolve(name: &str, leg: Option<LegValue>) -> Option<DurableKind> {
        let plain: fn(u64) -> DurableScenario = match (name, leg) {
            ("m2-ledger-wedge", Some(LegValue::LedgerWedge(leg))) => {
                return Some(DurableKind::LedgerWedge(leg));
            }
            ("m4-tier-budget", Some(LegValue::TierBudget(leg))) => {
                return Some(DurableKind::TierBudget(leg));
            }
            ("m4-epoch-marks", Some(LegValue::EpochMarks(leg))) => {
                return Some(DurableKind::EpochMarks(leg));
            }
            ("m2-durable", None) => DurableScenario::m2_durable,
            ("m2-clean-stop", None) => DurableScenario::m2_clean_stop,
            ("m2-device-budget", None) => DurableScenario::m2_device_budget,
            ("m2-mode-transition", None) => DurableScenario::m2_mode_transition,
            ("m2-reorder-window", None) => DurableScenario::m2_reorder_window,
            ("m2-fill-tick", None) => DurableScenario::m2_fill_tick,
            ("m2-fill-dense", None) => DurableScenario::m2_fill_tick_dense,
            ("m2-group-hold", None) => DurableScenario::m2_group_hold,
            ("m2-fua-pending", None) => DurableScenario::m2_fua_pending,
            ("m2-ckpt-refused", None) => DurableScenario::m2_ckpt_refused,
            ("m2-stop-tick", None) => DurableScenario::m2_stop_tick,
            ("m2-recycle", None) => DurableScenario::m2_recycle,
            ("m3-document", None) => DurableScenario::m3_document,
            _ => return None,
        };
        Some(DurableKind::Plain(plain))
    }

    fn scenario(self, seed: u64) -> DurableScenario {
        match self {
            DurableKind::Plain(make) => make(seed),
            DurableKind::LedgerWedge(leg) => DurableScenario::m2_ledger_wedge(seed, leg),
            DurableKind::TierBudget(leg) => DurableScenario::m4_tier_budget(seed, leg),
            DurableKind::EpochMarks(leg) => DurableScenario::m4_epoch_marks(seed, leg),
        }
    }
}

/// Shared durable runner: M2's key/value workload and M3's document
/// workload differ only in scenario construction. Both write replayable
/// per-seed results and a sweep manifest.
/// The cap census oracle's liveness, on its own machine-readable line: the
/// rows it read at the end of every life, and of those the rows that
/// crossed, which its crossing rule judged (node sums, ADR-0163 D2).
fn print_census(rows_checked: u64, rows_crossed: u64) {
    println!("inf-sim: cap_rows_checked={rows_checked} cap_rows_crossed={rows_crossed}");
}

/// The accept-retry oracle's reading on its leg (`--plant accept-broken`):
/// the broken-class parks, how each ended, and the first `AcceptArm`'s
/// timing against the park's retry deadline (node sums, ns).
fn print_accept_retry(report: &inf_sim::harness::accept_retry::RetryReport) {
    println!("inf-sim: accept_retry {report}");
}

/// `m4-extent-window`: one seeded leg, its disk-side counts and its verdict.
fn run_extent_window(seed: u64, leg: ExtentWindowLeg, verify: Verify) -> Exit {
    let run_one = |seed| {
        let scenario = inf_sim::ExtentWindowScenario::m4_extent_window(seed, leg);
        inf_sim::run_extent_window_scenario(&scenario)
    };
    driver::run_single(seed, verify, run_one, |report| {
        println!(
            "inf-sim: scenario m4-extent-window leg {} seed {seed:#x}: {} blob writes acked, {} \
         clients left, {} extent files created, peak {} open, {} frame holds, {} parks, {} \
         steps, hash {:#018x}",
            leg.name(),
            report.sets_acked,
            report.writers_left,
            report.files_created,
            report.handles_peak,
            report.frame_waits_extent,
            report.extent_parks,
            report.scheduler_steps,
            report.trace_hash
        );
        if let Some(row) = report.window_row {
            println!(
                "inf-sim: extent window cell 0: high_water {} of {}, crossings {}",
                row.high_water, row.entries_max, row.crossings
            );
        }
        print_census(report.cap_rows_checked, report.cap_rows_crossed);
        Ok(())
    })
}

#[allow(clippy::too_many_arguments)] // one linear CLI dispatch, like main
fn run_durable(
    scenario_name: &str,
    kind: DurableKind,
    seed: u64,
    plant: Plant,
    replay_canary: bool,
    lift_regime: bool,
    park_at_once: bool,
    determinism: Verify,
    sweep: Option<&Sweep>,
) {
    let run_one = |seed: u64| -> inf_sim::DurableReport {
        let mut scenario = kind.scenario(seed);
        scenario.plant = plant;
        scenario.replay_canary = replay_canary;
        scenario.lift_regime |= lift_regime;
        if park_at_once {
            scenario = scenario.park_at_once();
        }
        run_durable_scenario(&scenario)
    };

    let Some(sweep) = sweep else {
        let exit = driver::run_single(seed, determinism, run_one, |report| {
            println!(
                "inf-sim: scenario {scenario_name} seed {seed:#x}: {} commands, {} steps, {} keys \
                 audited, {} required ops, {} allowed-lost, {} equivalence checks, {} documents \
                 compared, {} corpus docs, cut classes {:?}, lift regime {} (tiered ops {}, \
                 indexed \
                 ops {}, sidecars loaded {}, stale slacks lifted {}; plant: cells {}, lifts {}, \
                 sidecars {}; torn plants {}), arms [ckpt_downgrades {} bound_splits {} waits_fill \
                 {} waits_group {} plant_fired {}], log oracles [idle_tick_violations {} \
                 frames_awaiting_max {} fsync_entries_max {} write_through_entries_max {} \
                 hold_episode_violations {}], clean stop [steps {} replay_records {}], trace {} \
                 bytes, hash {:#018x}",
                report.commands_done,
                report.scheduler_steps,
                report.audited_keys,
                report.required_ops,
                report.allowed_lost_ops,
                report.equivalence_checks,
                report.documents_compared,
                report.corpus_documents_used,
                report.cut_classes,
                report.lift_regime,
                report.lift_tiered_ops,
                report.lift_indexed_ops,
                report.lift_sidecars_loaded,
                report.stale_residue_slacks,
                report.lift_plants,
                report.lift_plant_lifts,
                report.lift_plant_sidecars,
                report.torn_plants,
                report.ckpt_downgrades,
                report.ckpt_bound_splits,
                report.frame_waits_fill,
                report.frame_waits_group,
                report.plant_fired,
                report.idle_tick_violations,
                report.frames_awaiting_max,
                report.fsync_entries_max,
                report.write_through_entries_max,
                report.hold_episode_violations,
                report.clean_stop_steps,
                report.clean_stop_replay_records,
                report.trace.len(),
                report.trace_hash
            );
            println!("inf-sim: sim_seconds={:.6} published=0 delivered=0", report.sim_seconds);
            print_census(report.cap_rows_checked, report.cap_rows_crossed);
            if let Some(stop) = &report.stop_tick {
                println!("inf-sim: {}", stop.describe());
            }
            if scenario_name == "m2-fill-dense" {
                println!("inf-sim: {}", report.timers.describe());
            }
            if park_at_once {
                println!("inf-sim: wake {:?}", report.wake);
            }
            if let Some(wedge) = &report.ledger_wedge {
                println!(
                    "inf-sim: ledger wedge leg {}: armed at step {:?}, hold from step {:?} to \
                     {:?}, \
                     seal folded at step {:?}, driver held at most {} fdatasync(s); segment ids \
                     inside the hold {:?}; frame_waits_ledger {} frame_waits_ack_map {} \
                     ledger_prealloc_deferrals {} fsync_entries_max {} write_through_entries_max \
                     {}",
                    wedge.leg.name(),
                    wedge.armed_at,
                    wedge.engaged_at,
                    wedge.lifted_at,
                    wedge.folded_at,
                    wedge.held_max,
                    wedge.ids_inside_hold,
                    wedge.frame_waits_ledger,
                    report.frame_waits_ack_map,
                    report.ledger_prealloc_deferrals,
                    report.fsync_entries_max,
                    report.write_through_entries_max
                );
            }
            for marks in &report.epoch_marks_cells {
                println!(
                    "inf-sim: epoch marks cell {}: high_water {} of {}, crossings {}",
                    marks.cell, marks.row.high_water, marks.row.entries_max, marks.row.crossings
                );
            }
            for cell in &report.tier_budget_cells {
                println!(
                    "inf-sim: tier class cell {}: io_budget_unattainable_tier_flush {} \
                     io_budget_deferrals_tier_flush {} io_budget_bytes_tier_flush {} \
                     io_budget_ops_tier_flush {} io_budget_overrun_bytes_tier_flush {} \
                     driver tier-file bytes {} ops {} (queued at the cut {} B {} ops; one pass at \
                     most {} B {} ops, a round at most 74: a wiring read, vacuous for the bound); \
                     turn: tier_flush_groups_deferred {} tier_group_wait_ns_max \
                     {} tier_turn_waits {} tier_turn_contended {} tier_plan_waits {} \
                     tier_rounds_planned {} tiering_flush_write_retries {}; (namespace, flushed, \
                     ro, tail) {:?}",
                    cell.cell,
                    cell.unattainable,
                    cell.deferrals,
                    cell.spent_bytes,
                    cell.spent_ops,
                    cell.overrun_bytes,
                    cell.driver_bytes,
                    cell.driver_ops,
                    cell.queued_bytes,
                    cell.queued_ops,
                    cell.pass_bytes_max,
                    cell.pass_ops_max,
                    cell.groups_deferred,
                    cell.group_wait_ns_max,
                    cell.turn_waits,
                    cell.turn_contended,
                    cell.plan_waits,
                    cell.rounds_planned,
                    cell.write_retries,
                    cell.flushed
                );
            }
            if report.stalled {
                eprintln!("inf-sim: STALL (seed {seed:#x})");
            }
            Ok(())
        });
        std::process::exit(exit.code());
    };

    let fold = DurableFold { plant, replay_canary, ..DurableFold::default() };
    std::process::exit(driver::run_sweep(scenario_name, sweep, fold, run_one).code());
}

/// The M2.5-S14 combined runner: one seed, or a sharded sweep. Mirrors
/// the shared durable runner (per-seed results + a manifest,
/// `violations=0` gate);
/// any failing seed replays byte-identically via `--seed`.
fn run_m2_combined(seed: u64, determinism: Verify, sweep: Option<&Sweep>) {
    let run_one = |seed: u64| -> inf_sim::CombinedReport {
        run_combined_scenario(&CombinedScenario::m2_combined(seed))
    };

    let Some(sweep) = sweep else {
        let exit = driver::run_single(seed, determinism, run_one, |report| {
            println!(
                "inf-sim: scenario m2-combined seed {seed:#x}: {} commands, {} steps, {} keys \
                 audited, {} memory keys audited, {} required ops, {} allowed-lost, {} published, \
                 {} delivered, stall-max-always {} ms, trace {} bytes, hash {:#018x}",
                report.commands_done,
                report.scheduler_steps,
                report.audited_keys,
                report.memory_keys_audited,
                report.required_ops,
                report.allowed_lost_ops,
                report.published,
                report.delivered,
                report.always_ack_latency_ms_max,
                report.trace.len(),
                report.trace_hash
            );
            println!(
                "inf-sim: sim_seconds={:.6} published={} delivered={}",
                report.sim_seconds, report.published, report.delivered
            );
            print_census(report.cap_rows_checked, report.cap_rows_crossed);
            if report.stalled {
                eprintln!("inf-sim: STALL (seed {seed:#x})");
            }
            Ok(())
        });
        std::process::exit(exit.code());
    };

    std::process::exit(driver::run_sweep("m2-combined", sweep, CombinedFold, run_one).code());
}

/// `m4-recovery`'s shard: the seed classes and arms that must actually occur
/// across it (ADR-0045 D4: coverage disclosed, never assumed).
#[derive(Default)]
struct RecoveryFold {
    refs: u64,
    images: u64,
    cuts_before: u64,
    flush_lag: u64,
    live_entries: u64,
    relocations: u64,
    files_retired: u64,
    files_unlinked: u64,
    boot_gc: u64,
    blobs: u64,
    blob_orphans: u64,
    blob_reclaims: u64,
    shadow_opened: u64,
    shadow_at_cut: u64,
    shadow_reformed: u64,
    shadow_same_key: u64,
    shadow_collision: u64,
    shadow_collide_ops: u64,
    shadow_settled_at_boot: u64,
    shadow_drain_checks: u64,
    shadow_multi_winners: u64,
    shadow_multi_dels: u64,
    shadow_twin_origin_rows: u64,
    shadow_twin_origins_covered: u64,
    shadow_held_rows: u64,
    shadow_held_reformed: u64,
    shadow_held_not_restored: u64,
    // ADR-0174 D6: the boots that demoted, the fitting boots the zero set
    // was checked on, and boot replay's counters.
    demoting_boots: u64,
    fitting_boots: u64,
    boot_replay: inf_store::ReplayCounters,
    writer_parks: u64,
    two_crash_removed: u64,
    two_crash_skipped: u64,
    // ADR-0174 D2 rule 6: the spec-variant seeds and their long records
    // (the pads are `boot_replay.pads_placed`).
    variant_seeds: u64,
    long_records: u64,
    seal_census: inf_sim::SealCensus,
}

impl Fold for RecoveryFold {
    type Report = inf_sim::RecoveryReport;

    fn absorb(&mut self, report: &Self::Report) {
        self.demoting_boots += report.demoting_boots;
        self.fitting_boots += report.fitting_boots_checked;
        self.boot_replay.absorb(report.boot_replay);
        self.writer_parks += report.writer_parks;
        self.two_crash_removed += report.two_crash_markers_removed;
        self.two_crash_skipped += report.two_crash_unit_past_window;
        self.variant_seeds += u64::from(report.spec_variant.is_some());
        self.long_records += report.long_records_written;
        self.seal_census.absorb(report.seal_census);
        self.shadow_held_rows += report.shadow_held_rows;
        self.shadow_held_reformed += report.shadow_held_reformed;
        self.shadow_held_not_restored += report.shadow_held_not_restored;
        self.shadow_multi_winners += report.shadow_multi_ticket_winners;
        self.shadow_multi_dels += report.shadow_multi_ticket_dels;
        self.shadow_twin_origin_rows += report.shadow_twin_origin_rows;
        self.shadow_twin_origins_covered += report.shadow_twin_origins_covered;
        self.shadow_opened += report.shadow_opened;
        self.shadow_at_cut += report.shadow_open_at_cut;
        self.shadow_reformed += report.shadow_reformed;
        self.shadow_same_key += report.shadow_same_key;
        self.shadow_collision += report.shadow_collision;
        self.shadow_collide_ops += report.shadow_collide_ops;
        self.shadow_settled_at_boot += report.shadow_settled_at_boot;
        self.shadow_drain_checks += report.shadow_drain_checks;
        self.refs += report.refs_emitted;
        self.images += report.images_emitted;
        self.cuts_before += report.cut_before_publish;
        self.flush_lag += report.flush_lag_lives;
        self.live_entries += report.live_entries_emitted;
        self.relocations += report.relocations;
        self.files_retired += report.files_retired;
        self.files_unlinked += report.files_unlinked;
        self.boot_gc += report.unlinks_left_to_boot_gc;
        self.blobs += report.blobs_written;
        self.blob_orphans += report.blob_orphans_planted;
        self.blob_reclaims += report.blob_extents_reclaimed;
    }

    fn coverage(&self) -> String {
        format!("{}{}", self.coverage_lives(), self.coverage_boots())
    }

    fn manifest_tail(&self) -> String {
        let Self {
            refs,
            images,
            cuts_before,
            flush_lag,
            live_entries,
            relocations,
            files_retired,
            files_unlinked,
            boot_gc,
            blobs,
            blob_orphans,
            blob_reclaims,
            shadow_opened,
            shadow_at_cut,
            shadow_reformed,
            shadow_same_key,
            shadow_collision,
            shadow_collide_ops,
            shadow_settled_at_boot,
            shadow_drain_checks,
            shadow_multi_winners,
            shadow_multi_dels,
            shadow_twin_origin_rows,
            shadow_twin_origins_covered,
            shadow_held_rows,
            shadow_held_reformed,
            shadow_held_not_restored,
            ..
        } = self;
        format!(
            "refs={refs} images={images} cut_before_publish={cuts_before} \
             flush_lag_lives={flush_lag} live_set_entries={live_entries} \
             relocations={relocations} files_retired={files_retired} \
             files_unlinked={files_unlinked} unlinks_boot_gc={boot_gc} \
             blobs={blobs} blob_orphans={blob_orphans} blob_reclaims={blob_reclaims} \
             shadow_opened={shadow_opened} shadow_at_cut={shadow_at_cut} \
             shadow_reformed={shadow_reformed} shadow_same_key={shadow_same_key} \
             shadow_collision={shadow_collision} shadow_collide_ops={shadow_collide_ops} \
             shadow_settled_at_boot={shadow_settled_at_boot} \
             shadow_drain_checks={shadow_drain_checks} \
             shadow_multi_winners={shadow_multi_winners} \
             shadow_multi_dels={shadow_multi_dels} \
             shadow_twin_origin_rows={shadow_twin_origin_rows} \
             shadow_twin_origins_covered={shadow_twin_origins_covered} \
             shadow_held_rows={shadow_held_rows} \
             shadow_held_reformed={shadow_held_reformed} \
             shadow_held_not_restored={shadow_held_not_restored}"
        )
    }

    fn shard_end_clauses(&self) -> Vec<String> {
        Vec::new()
    }
}

impl RecoveryFold {
    fn coverage_lives(&self) -> String {
        let Self {
            refs,
            images,
            cuts_before,
            flush_lag,
            live_entries,
            relocations,
            files_retired,
            files_unlinked,
            boot_gc,
            blobs,
            blob_orphans,
            blob_reclaims,
            shadow_opened,
            shadow_at_cut,
            shadow_reformed,
            shadow_same_key,
            shadow_collision,
            shadow_collide_ops,
            shadow_settled_at_boot,
            shadow_drain_checks,
            shadow_multi_winners,
            shadow_multi_dels,
            shadow_twin_origin_rows,
            shadow_twin_origins_covered,
            shadow_held_rows,
            shadow_held_reformed,
            shadow_held_not_restored,
            ..
        } = self;
        format!(
            "{refs} refs, {images} images, \
             {cuts_before} cut-before-publish lives, {flush_lag} flush-lag lives, \
             {live_entries} live-set entries, {relocations} relocations, \
             {files_retired} retired, {files_unlinked} unlinked, \
             {boot_gc} left-to-boot-gc, {blobs} blobs, {blob_orphans} orphans-planted, \
             {blob_reclaims} blob-reclaims; shadow {shadow_opened} opened, \
             {shadow_at_cut} open at a cut, {shadow_reformed} re-formed by recovery, \
             {shadow_same_key} same-key / {shadow_collision} collision verdicts; \
             {shadow_collide_ops} ops on crafted colliding pairs, \
             {shadow_settled_at_boot} slots settled at boot, {shadow_drain_checks} \
             DBSIZE-drain checks; rebuilt multi-ticket winners {shadow_multi_winners} \
             ({shadow_multi_dels} DELs drained them), twin-with-origins rows \
             {shadow_twin_origin_rows} ({shadow_twin_origins_covered} origins covered by \
             DEL markers), held-across-the-walk rows {shadow_held_rows} \
             ({shadow_held_reformed} re-formed at boot, {shadow_held_not_restored} not \
             restored by an older manifest); "
        )
    }

    fn coverage_boots(&self) -> String {
        let Self {
            demoting_boots,
            fitting_boots,
            boot_replay,
            writer_parks,
            two_crash_removed,
            two_crash_skipped,
            variant_seeds,
            long_records,
            seal_census,
            ..
        } = self;
        format!(
            "boots {demoting_boots} demoting / \
             {fitting_boots} fitting checked, boot replay {} demote steps / {} tier bytes / \
             {} settle reads / {} same-key / {} distinct / {} deletes verified / {} blob \
             releases, {writer_parks} writer parks, {two_crash_removed} two-crash refs \
             removed by a marker ({two_crash_skipped} rows skipped: unit past the raised \
             window); spec-variant seeds {variant_seeds}, {long_records} long records, {} \
             pads placed; boot files censused {} (capacity {} / gap {} / shutdown {} \
             seals), {} page pads crossed, against a summed bound of {} (closest boot: {})",
            boot_replay.demote_steps,
            boot_replay.tier_bytes,
            boot_replay.settle_reads,
            boot_replay.settled_same_key,
            boot_replay.settled_distinct,
            boot_replay.deletes_verified,
            boot_replay.blob_releases,
            boot_replay.pads_placed,
            seal_census.files,
            seal_census.capacity_seals,
            seal_census.gap_seals,
            seal_census.shutdown_seals,
            seal_census.page_pads,
            seal_census.bound,
            closest_boot(seal_census.closest)
        )
    }
}

/// `m4-tiered`'s shard: demotion, cold reads, blobs, refusals and both
/// drop-race outcomes must actually occur across it (ADR-0045 D4).
#[derive(Default)]
struct TieredFold {
    commands: u64,
    audited: u64,
    flushed_pre_cut: u64,
    cold_resolves: u64,
    blob_sets: u64,
    race_replans: u64,
    dir_open_fault_seeds: u64,
    dir_open_faults_fired: u64,
    tier_read_fault_seeds: u64,
    tier_read_faults_fired: u64,
    tier_read_error_replies: u64,
    tier_read_faults_unconsumed: u64,
    scan_cold_pages: u64,
    scan_cold_reads: u64,
    ckpt_downgrades: u64,
    ckpt_bound_splits: u64,
    diskfull_refusals: u64,
    drop_values: u64,
    drop_other: u64,
    drop_reboots: u64,
    drop_reboot_residue: u64,
    drop_cut_whole: u64,
    drop_cut_swept: u64,
    shadow_seeds: u64,
    shadow_created: u64,
    shadow_same_key: u64,
    shadow_collision: u64,
    shadow_fallbacks: u64,
    shadow_stale: u64,
    shadow_at_cut: u64,
    collide_tickets: u64,
    collide_verdicts: u64,
    collide_ticketed: u64,
    collide_drains: u64,
    collide_scan_twins: u64,
    open_seeds: u64,
    open_tickets: u64,
    open_drains: u64,
    open_reads: u64,
    open_scan_twins: u64,
    open_retargeted: u64,
    open_forced_deletes: u64,
    open_ticketed: u64,
    open_collisions: u64,
    open_read_faults: u64,
    open_settled_without_read: u64,
    rebuilt_seeds: u64,
    rebuilt_tickets: u64,
    rebuilt_multi_dels: u64,
    rebuilt_same_key_twins: u64,
    rebuilt_fill_sets: u64,
    rebuilt_reboots: u64,
    promoted_extent_pairs: u64,
}

impl Fold for TieredFold {
    type Report = inf_sim::TieredNodeReport;

    fn absorb(&mut self, report: &Self::Report) {
        self.absorb_tickets(report);
        self.commands += report.commands_done;
        self.audited += report.audited_keys;
        self.flushed_pre_cut += report.flushed_pre_cut_bytes;
        self.cold_resolves += report.cold_resolves;
        self.blob_sets += report.blob_sets;
        self.race_replans += report.race_replans;
        self.dir_open_fault_seeds += u64::from(report.dir_open_fault_arm);
        self.dir_open_faults_fired += report.dir_open_faults_fired;
        self.tier_read_fault_seeds += u64::from(report.tier_read_fault_arm);
        self.tier_read_faults_fired += report.tier_read_faults_fired;
        self.tier_read_error_replies += report.tier_read_error_replies;
        self.tier_read_faults_unconsumed += report.tier_read_faults_unconsumed;
        self.scan_cold_pages += report.scan_cold_pages;
        self.scan_cold_reads += report.scan_cold_reads;
        self.ckpt_downgrades += report.ckpt_downgrades;
        self.ckpt_bound_splits += report.ckpt_bound_splits;
        self.diskfull_refusals += report.diskfull_refusals;
        self.drop_values += report.drop_replies_value;
        self.drop_other += report.drop_replies_other;
        self.drop_reboots += u64::from(report.drop_reboot_ok);
        self.drop_reboot_residue += u64::from(report.drop_reboot_manifest_residue);
        match report.drop_cut_outcome {
            inf_sim::tiered::DropCutOutcome::Whole => self.drop_cut_whole += 1,
            inf_sim::tiered::DropCutOutcome::Swept => self.drop_cut_swept += 1,
            inf_sim::tiered::DropCutOutcome::NotReached => {}
        }
    }

    fn coverage(&self) -> String {
        format!("{}{}", self.coverage_data_plane(), self.coverage_tickets())
    }

    fn manifest_tail(&self) -> String {
        format!("{} {}", self.manifest_data_plane(), self.manifest_tickets())
    }

    /// The tier-read fault arm's engagement over the shard: armed seeds
    /// whose audits never met a fault prove nothing. A single run's forced
    /// arm checks its own seed instead.
    fn shard_end_clauses(&self) -> Vec<String> {
        let mut fired = Vec::new();
        if self.tier_read_fault_seeds > 0 && self.tier_read_faults_fired == 0 {
            fired.push(format!(
                "TIER-READ FAULT ARM VACUOUS across the shard: {} armed seeds, no fault consumed",
                self.tier_read_fault_seeds
            ));
        }
        fired
    }
}

impl TieredFold {
    /// The shadow-ticket, forced-collision, open-ticket and rebuilt-row arms.
    fn absorb_tickets(&mut self, report: &inf_sim::TieredNodeReport) {
        self.rebuilt_seeds += u64::from(report.rebuilt_rows);
        self.rebuilt_tickets += report.rebuilt_tickets;
        self.rebuilt_multi_dels += report.rebuilt_multi_dels;
        self.rebuilt_same_key_twins += report.rebuilt_same_key_twins;
        self.rebuilt_fill_sets += report.rebuilt_fill_sets;
        self.rebuilt_reboots += report.rebuilt_reboots;
        self.promoted_extent_pairs += report.promoted_extent_pairs;
        self.open_seeds += u64::from(report.open_rows);
        self.open_tickets += report.open_tickets;
        self.open_drains += report.open_dbsize_drains;
        self.open_reads += report.open_dbsize_reads;
        self.open_scan_twins += report.open_scan_twins;
        self.open_retargeted += report.open_retargeted;
        self.open_forced_deletes += report.open_forced_deletes;
        self.open_ticketed += report.open_ticketed_fallbacks;
        self.open_collisions += report.open_collision_verdicts;
        self.open_read_faults += report.open_read_fault_errors;
        self.open_settled_without_read += report.open_settled_without_read;
        self.collide_tickets += report.collide_tickets;
        self.collide_verdicts += report.collide_verdicts;
        self.collide_ticketed += report.collide_ticketed_fallbacks;
        self.collide_drains += report.collide_dbsize_drains;
        self.collide_scan_twins += report.collide_scan_twins;
        self.shadow_seeds += u64::from(report.shadow_arm);
        self.shadow_created += report.shadow_created;
        self.shadow_same_key += report.shadow_resolved_same_key;
        self.shadow_collision += report.shadow_resolved_collision;
        self.shadow_fallbacks += report.shadow_fallbacks;
        self.shadow_stale += report.shadow_stale;
        self.shadow_at_cut += report.shadow_pending_at_cut;
    }

    fn coverage_data_plane(&self) -> String {
        let Self {
            commands,
            audited,
            flushed_pre_cut,
            cold_resolves,
            blob_sets,
            race_replans,
            dir_open_fault_seeds,
            dir_open_faults_fired,
            tier_read_fault_seeds,
            tier_read_faults_fired,
            tier_read_error_replies,
            tier_read_faults_unconsumed,
            scan_cold_pages,
            scan_cold_reads,
            ckpt_downgrades,
            ckpt_bound_splits,
            diskfull_refusals,
            drop_values,
            drop_other,
            drop_reboots,
            drop_reboot_residue,
            drop_cut_whole,
            drop_cut_swept,
            ..
        } = self;
        format!(
            "{commands} commands, {audited} \
             keys audited, {flushed_pre_cut} B flushed pre-cut, {cold_resolves} cold \
             resolves, {blob_sets} blob sets, {race_replans} blob-key race replans, \
             dir-open fault armed on {dir_open_fault_seeds} seeds ({dir_open_faults_fired} \
             fired), tier-read EIO armed on {tier_read_fault_seeds} seeds \
             ({tier_read_faults_fired} fired, {tier_read_error_replies} typed replies, \
             {tier_read_faults_unconsumed} unconsumed), SCAN batching oracle on \
             {scan_cold_pages} cold pages ({scan_cold_reads} cold intents), \
             ckpt arms [downgrades {ckpt_downgrades} bound_splits \
             {ckpt_bound_splits}], {diskfull_refusals} DISKFULL refusals, \
             drop-race {drop_values} values / {drop_other} typed-other; post-drop reboots \
             {drop_reboots} ({drop_reboot_residue} with MANIFEST residue), cut inside DROP: \
             {drop_cut_whole} whole / {drop_cut_swept} swept (ADR-0100); "
        )
    }

    fn coverage_tickets(&self) -> String {
        let Self {
            shadow_seeds,
            shadow_created,
            shadow_same_key,
            shadow_collision,
            shadow_fallbacks,
            shadow_stale,
            shadow_at_cut,
            collide_tickets,
            collide_verdicts,
            collide_ticketed,
            collide_drains,
            collide_scan_twins,
            open_seeds,
            open_tickets,
            open_drains,
            open_reads,
            open_scan_twins,
            open_retargeted,
            open_forced_deletes,
            open_ticketed,
            open_collisions,
            open_read_faults,
            open_settled_without_read,
            rebuilt_seeds,
            rebuilt_tickets,
            rebuilt_multi_dels,
            rebuilt_same_key_twins,
            rebuilt_fill_sets,
            rebuilt_reboots,
            promoted_extent_pairs,
            ..
        } = self;
        format!(
            "shadow arm on \
             {shadow_seeds} seeds: {shadow_created} tickets ({shadow_at_cut} open at the \
             cut), {shadow_same_key} same-key / {shadow_collision} collision verdicts, \
             {shadow_stale} stale, {shadow_fallbacks} fallbacks; forced collisions: \
             {collide_tickets} tickets, {collide_verdicts} collision verdicts, \
             {collide_ticketed} ticketed fallbacks, {collide_drains} DBSIZE drains, \
             {collide_scan_twins} SCAN twins; open-ticket rows on {open_seeds} seeds: \
             {open_tickets} tickets held open, {open_drains} DBSIZE drains reading \
             {open_reads} twins, {open_scan_twins} SCAN twins, {open_retargeted} \
             retargets, {open_forced_deletes} read-free forced deletes, {open_ticketed} \
             ticketed fallbacks, {open_collisions} collision verdicts, {open_read_faults} \
             injected read errors relayed, {open_settled_without_read} settled without a \
             read after resume; rebuilt multi-ticket rows on {rebuilt_seeds} seeds: \
             {rebuilt_tickets} tickets rebuilt, {rebuilt_multi_dels} DELs draining several, \
             {rebuilt_same_key_twins} same-key twins among them, {rebuilt_fill_sets} filler \
             writes, {rebuilt_reboots} reboots; {promoted_extent_pairs} cold blob GET pairs \
             re-resolved onto a promoted reference"
        )
    }

    fn manifest_data_plane(&self) -> String {
        let Self {
            commands,
            audited,
            flushed_pre_cut,
            cold_resolves,
            blob_sets,
            dir_open_fault_seeds,
            dir_open_faults_fired,
            tier_read_fault_seeds,
            tier_read_faults_fired,
            tier_read_error_replies,
            tier_read_faults_unconsumed,
            scan_cold_pages,
            scan_cold_reads,
            ckpt_downgrades,
            ckpt_bound_splits,
            diskfull_refusals,
            drop_values,
            drop_other,
            drop_reboots,
            drop_reboot_residue,
            drop_cut_whole,
            drop_cut_swept,
            ..
        } = self;
        format!(
            "commands={commands} keys_audited={audited} \
             flushed_pre_cut={flushed_pre_cut} cold_resolves={cold_resolves} \
             blob_sets={blob_sets} dir_open_fault_seeds={dir_open_fault_seeds} \
             dir_open_faults_fired={dir_open_faults_fired} \
             tier_read_fault_seeds={tier_read_fault_seeds} \
             tier_read_faults_fired={tier_read_faults_fired} \
             tier_read_error_replies={tier_read_error_replies} \
             tier_read_faults_unconsumed={tier_read_faults_unconsumed} \
             scan_cold_pages={scan_cold_pages} scan_cold_reads={scan_cold_reads} \
             ckpt_downgrades={ckpt_downgrades} ckpt_bound_splits={ckpt_bound_splits} \
             diskfull_refusals={diskfull_refusals} \
             drop_values={drop_values} drop_other={drop_other} \
             drop_reboots={drop_reboots} drop_reboot_residue={drop_reboot_residue} \
             drop_cut_whole={drop_cut_whole} drop_cut_swept={drop_cut_swept}"
        )
    }

    fn manifest_tickets(&self) -> String {
        let Self {
            shadow_seeds,
            shadow_created,
            shadow_same_key,
            shadow_collision,
            shadow_fallbacks,
            shadow_stale,
            shadow_at_cut,
            collide_tickets,
            collide_verdicts,
            collide_ticketed,
            collide_drains,
            collide_scan_twins,
            open_seeds,
            open_tickets,
            open_drains,
            open_reads,
            open_scan_twins,
            open_retargeted,
            open_forced_deletes,
            open_ticketed,
            open_collisions,
            open_read_faults,
            open_settled_without_read,
            rebuilt_seeds,
            rebuilt_tickets,
            rebuilt_multi_dels,
            rebuilt_same_key_twins,
            rebuilt_fill_sets,
            rebuilt_reboots,
            promoted_extent_pairs,
            ..
        } = self;
        format!(
            "shadow_seeds={shadow_seeds} shadow_created={shadow_created} \
             shadow_at_cut={shadow_at_cut} shadow_same_key={shadow_same_key} \
             shadow_collision={shadow_collision} shadow_stale={shadow_stale} \
             shadow_fallbacks={shadow_fallbacks} collide_tickets={collide_tickets} \
             collide_verdicts={collide_verdicts} collide_ticketed={collide_ticketed} \
             collide_drains={collide_drains} collide_scan_twins={collide_scan_twins} \
             open_seeds={open_seeds} open_tickets={open_tickets} \
             open_drains={open_drains} open_reads={open_reads} \
             open_scan_twins={open_scan_twins} open_retargeted={open_retargeted} \
             open_forced_deletes={open_forced_deletes} open_ticketed={open_ticketed} \
             open_collisions={open_collisions} open_read_faults={open_read_faults} \
             open_settled_without_read={open_settled_without_read} \
             rebuilt_seeds={rebuilt_seeds} rebuilt_tickets={rebuilt_tickets} \
             rebuilt_multi_dels={rebuilt_multi_dels} \
             rebuilt_same_key_twins={rebuilt_same_key_twins} \
             rebuilt_fill_sets={rebuilt_fill_sets} rebuilt_reboots={rebuilt_reboots} \
             promoted_extent_pairs={promoted_extent_pairs}"
        )
    }
}

/// `m2-ns-ddl-race`'s shard: the crossings and refused legs that must occur.
#[derive(Default)]
struct NsDdlFold {
    found: u64,
    phantoms: u64,
    partial: u64,
    skips: u64,
    crossings: u64,
}

impl Fold for NsDdlFold {
    type Report = inf_sim::NsDdlRaceReport;

    fn absorb(&mut self, report: &Self::Report) {
        self.found += u64::from(report.drop_found);
        self.crossings += report.crossings;
        let served = u64::from(report.served_before_cut);
        if served != 0 && served != 4 {
            self.phantoms += 1;
        }
        self.partial += u64::from(report.partial_served);
        self.skips += report.skipped_unknown_ns;
    }

    fn coverage(&self) -> String {
        let Self { found, phantoms, partial, skips, crossings } = self;
        format!(
            "{crossings} crossings (origin frozen mid-fan), {found} DROPs found the namespace, \
             {phantoms} phantom namespaces, {partial} cells serving a refused CREATE, {skips} \
             untombstoned unknown-ns skips"
        )
    }

    fn manifest_tail(&self) -> String {
        let Self { found, phantoms, partial, skips, crossings } = self;
        format!(
            "crossings={crossings} drops_found={found} phantom_namespaces={phantoms} \
             partial_served={partial} unknown_ns_skips={skips}"
        )
    }

    fn shard_end_clauses(&self) -> Vec<String> {
        Vec::new()
    }
}

/// `m2-ns-create-window`'s shard: the probes that met the held window.
#[derive(Default)]
struct NsCreateFold {
    attempts: u64,
    refused: u64,
    acked: u64,
    skips: u64,
    held: u64,
}

impl Fold for NsCreateFold {
    type Report = inf_sim::NsCreateWindowReport;

    fn absorb(&mut self, report: &Self::Report) {
        self.attempts += report.use_attempts;
        self.refused += report.use_refused;
        self.acked += report.acked_in_window;
        self.skips += report.skipped_unknown_ns;
        self.held += report.held_steps;
    }

    fn coverage(&self) -> String {
        let Self { attempts, refused, acked, skips, held } = self;
        format!(
            "{attempts} USE attempts in the window ({refused} refused, {acked} writes acked), \
             {skips} untombstoned unknown-ns skips, {held} held steps"
        )
    }

    fn manifest_tail(&self) -> String {
        let Self { attempts, refused, acked, skips, held } = self;
        format!(
            "use_attempts={attempts} use_refused={refused} acked_in_window={acked} \
             unknown_ns_skips={skips} held_steps={held}"
        )
    }

    fn shard_end_clauses(&self) -> Vec<String> {
        Vec::new()
    }
}

/// The durable runner's shard: the cut classes, the frame pipeline, the
/// device budget, the log oracles, recycling and the lift regime, each
/// disclosed so a sweep cannot claim coverage the random process never
/// reached (ADR-0045 D4).
#[derive(Default)]
struct DurableFold {
    plant: Plant,
    replay_canary: bool,
    equivalence_checks: u64,
    documents_compared: u64,
    corpus_documents: u64,
    cut_classes: std::collections::BTreeMap<&'static str, u64>,
    // Frame-pipeline coverage (ADR-0087 D7): seeds whose cells reached
    // ≥ 2 frames in flight, the deepest seen, and the bounded waits.
    pipelined_seeds: u64,
    depth_max: u64,
    waits_barrier: u64,
    waits_rotation: u64,
    waits_reorder: u64,
    ckpt_downgrades: u64,
    bound_splits: u64,
    waits_fill: u64,
    waits_group: u64,
    // The log oracles: idle-tick violations summed, map depth and ledger
    // entries as shard maxima.
    idle_tick_violations: u64,
    frames_awaiting_max: u64,
    fsync_entries_max: u64,
    write_through_entries_max: u64,
    hold_episode_violations: u64,
    // Device-budget coverage (ADR-0088 D8): background bytes granted,
    // deferrals issued, seal-pace waits, worst frame-write latency.
    budget_bytes: u64,
    budget_deferrals: u64,
    budget_unattainable: u64,
    block_wait_ns_max: u64,
    waits_pace: u64,
    stall_max_us: u64,
    // Transition coverage (ADR-0086 D4 as amended): packed tails reopened
    // under a `Direct` rotor at the `m2-mode-transition` restart.
    reopened_packed_tails: u64,
    // Recycling coverage (ADR-0090 D5): segments recycled, pool misses +
    // fallbacks, rotations, and residue the reboots proved.
    recycled: u64,
    recycle_misses: u64,
    recycle_fallbacks: u64,
    recycle_served: u64,
    open_unreached: u64,
    recycle_sentinels: u64,
    clean_stop_torn_tails: u64,
    rotations: u64,
    residue_slacks: u64,
    // Pool-wait coverage (ADR-0090 D9): both outcomes must occur in a
    // sweep that claims to cover the wait.
    waits_started: u64,
    waits_satisfied: u64,
    waits_expired: u64,
    inline_preallocs: u64,
    // The lift regime (F-L14-01): seeds that ran it, the records its
    // writers landed behind the lift, the sidecars the final boot loaded,
    // and — every seed — the residue slacks the final boot lifted past.
    lift_seeds: u64,
    lift_tiered_ops: u64,
    lift_indexed_ops: u64,
    lift_sidecars_loaded: u64,
    lift_plants: u64,
    lift_plant_lifts: u64,
    lift_plant_sidecars: u64,
    stale_slacks: u64,
}

impl Fold for DurableFold {
    type Report = inf_sim::DurableReport;

    fn absorb(&mut self, report: &Self::Report) {
        self.absorb_log(report);
        self.absorb_recycling(report);
        self.equivalence_checks += report.equivalence_checks;
        self.documents_compared += report.documents_compared;
        self.corpus_documents += report.corpus_documents_used;
        for class in &report.cut_classes {
            *self.cut_classes.entry(class).or_default() += 1;
        }
    }

    fn coverage(&self) -> String {
        format!("{}{}", self.coverage_log(), self.coverage_recycling())
    }

    fn manifest_tail(&self) -> String {
        format!("{} {}", self.manifest_log(), self.manifest_recycling())
    }

    fn shard_end_clauses(&self) -> Vec<String> {
        Vec::new()
    }
}

impl DurableFold {
    /// The frame pipeline, the device budget and the log oracles.
    fn absorb_log(&mut self, report: &inf_sim::DurableReport) {
        self.pipelined_seeds += u64::from(report.frames_in_flight_max >= 2);
        self.depth_max = self.depth_max.max(report.frames_in_flight_max);
        self.waits_barrier += report.frame_waits_barrier;
        self.waits_rotation += report.frame_waits_rotation;
        self.waits_reorder += report.frame_waits_reorder;
        self.ckpt_downgrades += report.ckpt_downgrades;
        self.bound_splits += report.ckpt_bound_splits;
        self.waits_fill += report.frame_waits_fill;
        self.waits_group += report.frame_waits_group;
        self.idle_tick_violations += report.idle_tick_violations;
        self.frames_awaiting_max = self.frames_awaiting_max.max(report.frames_awaiting_max);
        self.fsync_entries_max = self.fsync_entries_max.max(report.fsync_entries_max);
        self.write_through_entries_max =
            self.write_through_entries_max.max(report.write_through_entries_max);
        self.hold_episode_violations += report.hold_episode_violations;
        self.budget_bytes += report.budget_background_bytes;
        self.budget_deferrals += report.budget_deferrals;
        self.budget_unattainable += report.budget_unattainable;
        self.block_wait_ns_max = self.block_wait_ns_max.max(report.ckpt_block_wait_ns_max);
        self.waits_pace += report.frame_waits_pace;
        self.stall_max_us = self.stall_max_us.max(report.write_stall_max_us);
        self.reopened_packed_tails += report.reopened_packed_tails;
    }

    /// Recycling, the pool wait and the lift regime.
    fn absorb_recycling(&mut self, report: &inf_sim::DurableReport) {
        self.recycled += report.segments_recycled;
        self.recycle_misses += report.recycle_misses;
        self.recycle_fallbacks += report.recycle_fallbacks;
        self.recycle_served += report.recycle_served;
        self.open_unreached += u64::from(report.recycle_open_unreached);
        self.recycle_sentinels += report.recycle_sentinels;
        self.clean_stop_torn_tails += report.clean_stop_torn_tails;
        self.rotations += report.segment_rotations;
        self.residue_slacks += report.recycled_residue_slacks;
        self.waits_started += report.recycle_waits_started;
        self.waits_satisfied += report.recycle_waits_satisfied;
        self.waits_expired += report.recycle_waits_expired;
        self.inline_preallocs += report.segment_inline_preallocs;
        self.lift_seeds += u64::from(report.lift_regime);
        self.lift_tiered_ops += report.lift_tiered_ops;
        self.lift_indexed_ops += report.lift_indexed_ops;
        self.lift_sidecars_loaded += report.lift_sidecars_loaded;
        self.stale_slacks += report.stale_residue_slacks;
        self.lift_plants += report.lift_plants;
        self.lift_plant_lifts += report.lift_plant_lifts;
        self.lift_plant_sidecars += report.lift_plant_sidecars;
    }

    /// The cut-class distribution, `class:count` in class order.
    fn classes(&self) -> String {
        let classes: Vec<String> =
            self.cut_classes.iter().map(|(class, count)| format!("{class}:{count}")).collect();
        classes.join(" ")
    }

    fn coverage_log(&self) -> String {
        let Self {
            equivalence_checks,
            documents_compared,
            pipelined_seeds,
            depth_max,
            waits_barrier,
            waits_rotation,
            waits_reorder,
            budget_bytes,
            budget_deferrals,
            budget_unattainable,
            block_wait_ns_max,
            waits_pace,
            stall_max_us,
            reopened_packed_tails,
            ckpt_downgrades,
            bound_splits,
            waits_fill,
            waits_group,
            idle_tick_violations,
            frames_awaiting_max,
            fsync_entries_max,
            write_through_entries_max,
            hold_episode_violations,
            ..
        } = self;
        format!(
            "{equivalence_checks} equivalence \
             checks, {documents_compared} documents compared, cut classes [{}], frame pipeline \
             [pipelined_seeds:{pipelined_seeds} depth_max:{depth_max} \
             waits_barrier:{waits_barrier} \
             waits_rotation:{waits_rotation} waits_reorder:{waits_reorder}], device budget \
             [background_bytes:{budget_bytes} \
             deferrals:{budget_deferrals} unattainable:{budget_unattainable} \
             ckpt_block_wait_ns_max:{block_wait_ns_max} waits_pace:{waits_pace} \
             write_stall_max_us:{stall_max_us}], \
             reopened_packed_tails:{reopened_packed_tails} ckpt_downgrades:{ckpt_downgrades} \
             bound_splits:{bound_splits} waits_fill:{waits_fill} waits_group:{waits_group}, log \
             oracles \
             [idle_tick_violations:{idle_tick_violations} \
             frames_awaiting_max:{frames_awaiting_max} \
             fsync_entries_max:{fsync_entries_max} \
             write_through_entries_max:{write_through_entries_max} \
             hold_episode_violations:{hold_episode_violations}], ",
            self.classes()
        )
    }

    fn coverage_recycling(&self) -> String {
        let Self {
            recycled,
            recycle_misses,
            recycle_fallbacks,
            recycle_served,
            open_unreached,
            rotations,
            residue_slacks,
            waits_started,
            waits_satisfied,
            waits_expired,
            inline_preallocs,
            lift_seeds,
            lift_tiered_ops,
            lift_indexed_ops,
            lift_sidecars_loaded,
            stale_slacks,
            lift_plants,
            lift_plant_lifts,
            lift_plant_sidecars,
            ..
        } = self;
        format!(
            "recycling [recycled:{recycled} misses:{recycle_misses} \
             fallbacks:{recycle_fallbacks} served:{recycle_served} \
             open_unreached:{open_unreached} rotations:{rotations} residue_slacks:{residue_slacks} \
             waits_started:{waits_started} waits_satisfied:{waits_satisfied} \
             waits_expired:{waits_expired} inline_preallocs:{inline_preallocs}], lift regime \
             [seeds:{lift_seeds} tiered_ops:{lift_tiered_ops} indexed_ops:{lift_indexed_ops} \
             sidecars_loaded:{lift_sidecars_loaded} stale_slacks:{stale_slacks} \
             plants:{lift_plants} plant_lifts:{lift_plant_lifts} \
             plant_sidecars:{lift_plant_sidecars}]"
        )
    }

    fn manifest_log(&self) -> String {
        let Self {
            plant,
            replay_canary,
            equivalence_checks,
            documents_compared,
            corpus_documents,
            pipelined_seeds,
            depth_max,
            waits_barrier,
            waits_rotation,
            waits_reorder,
            budget_bytes,
            budget_deferrals,
            budget_unattainable,
            block_wait_ns_max,
            waits_pace,
            stall_max_us,
            reopened_packed_tails,
            ckpt_downgrades,
            bound_splits,
            waits_fill,
            waits_group,
            idle_tick_violations,
            frames_awaiting_max,
            fsync_entries_max,
            write_through_entries_max,
            hold_episode_violations,
            ..
        } = self;
        format!(
            "plant={plant:?} replay_canary={replay_canary} \
             equivalence_checks={equivalence_checks} \
             documents_compared={documents_compared} corpus_documents={corpus_documents} \
             cut_classes=[{}] pipelined_seeds={pipelined_seeds} depth_max={depth_max} \
             waits_barrier={waits_barrier} waits_rotation={waits_rotation} \
             waits_reorder={waits_reorder} budget_background_bytes={budget_bytes} \
             budget_deferrals={budget_deferrals} budget_unattainable={budget_unattainable} \
             ckpt_block_wait_ns_max={block_wait_ns_max} \
             waits_pace={waits_pace} write_stall_max_us={stall_max_us} \
             reopened_packed_tails={reopened_packed_tails} ckpt_downgrades={ckpt_downgrades} \
             ckpt_bound_splits={bound_splits} waits_fill={waits_fill} waits_group={waits_group} \
             idle_tick_violations={idle_tick_violations} frames_awaiting_max={frames_awaiting_max} \
             fsync_entries_max={fsync_entries_max} \
             write_through_entries_max={write_through_entries_max} \
             hold_episode_violations={hold_episode_violations}",
            self.classes()
        )
    }

    fn manifest_recycling(&self) -> String {
        let Self {
            recycled,
            recycle_misses,
            recycle_fallbacks,
            recycle_served,
            open_unreached,
            recycle_sentinels,
            clean_stop_torn_tails,
            rotations,
            residue_slacks,
            waits_started,
            waits_satisfied,
            waits_expired,
            inline_preallocs,
            lift_seeds,
            lift_tiered_ops,
            lift_indexed_ops,
            lift_sidecars_loaded,
            stale_slacks,
            lift_plants,
            lift_plant_lifts,
            lift_plant_sidecars,
            ..
        } = self;
        format!(
            "segments_recycled={recycled} recycle_misses={recycle_misses} \
             recycle_fallbacks={recycle_fallbacks} recycle_served={recycle_served} \
             recycle_open_unreached={open_unreached} recycle_sentinels={recycle_sentinels} \
             clean_stop_torn_tails={clean_stop_torn_tails} segment_rotations={rotations} \
             recycled_residue_slacks={residue_slacks} recycle_waits_started={waits_started} \
             recycle_waits_satisfied={waits_satisfied} recycle_waits_expired={waits_expired} \
             segment_inline_preallocs={inline_preallocs} lift_seeds={lift_seeds} \
             lift_tiered_ops={lift_tiered_ops} lift_indexed_ops={lift_indexed_ops} \
             lift_sidecars_loaded={lift_sidecars_loaded} stale_residue_slacks={stale_slacks} \
             lift_plants={lift_plants} lift_plant_lifts={lift_plant_lifts} \
             lift_plant_sidecars={lift_plant_sidecars}"
        )
    }
}

/// `m2-combined`'s shard counts nothing beyond the driver's: its published
/// and delivered counts ride the `sim_seconds=` line.
struct CombinedFold;

impl Fold for CombinedFold {
    type Report = inf_sim::CombinedReport;

    fn absorb(&mut self, _report: &Self::Report) {}

    fn coverage(&self) -> String {
        "the published and delivered counts are on the sim_seconds line".to_owned()
    }

    fn manifest_tail(&self) -> String {
        String::new()
    }

    fn shard_end_clauses(&self) -> Vec<String> {
        Vec::new()
    }
}

/// `m2-ns-drop-window`'s shard: the acked keys served after a cut before
/// the answer, and the cut class each seed picked.
#[derive(Default)]
struct NsDropFold {
    keys: u64,
    cuts: std::collections::BTreeMap<String, u64>,
}

impl Fold for NsDropFold {
    type Report = inf_sim::NsDropWindowReport;

    fn absorb(&mut self, report: &Self::Report) {
        self.keys += report.keys_served;
        *self.cuts.entry(format!("{:?}", report.cut)).or_default() += 1;
    }

    fn coverage(&self) -> String {
        format!(
            "{} acked keys served after a cut before the answer, cut classes [{}]",
            self.keys,
            self.cut_classes()
        )
    }

    fn manifest_tail(&self) -> String {
        format!("keys_served={} cut_classes=[{}]", self.keys, self.cut_classes())
    }

    fn shard_end_clauses(&self) -> Vec<String> {
        Vec::new()
    }
}

impl NsDropFold {
    /// The cut-class distribution, `class:count` in class order.
    fn cut_classes(&self) -> String {
        let classes: Vec<String> =
            self.cuts.iter().map(|(class, count)| format!("{class}:{count}")).collect();
        classes.join(" ")
    }
}

#[cfg(test)]
mod tests {
    use inf_sim::driver::Fold;

    /// The tiered fold's shard-end clause: armed seeds whose audits met no
    /// fault fire it, and the controls (an arm that fired, or no arm) hold.
    #[test]
    fn the_tiered_arm_that_never_fired_is_a_shard_violation() {
        let vacuous = super::TieredFold { tier_read_fault_seeds: 3, ..Default::default() };
        let fired = vacuous.shard_end_clauses();
        assert_eq!(fired.len(), 1, "{fired:?}");
        assert!(fired[0].contains("TIER-READ FAULT ARM VACUOUS across the shard: 3 armed seeds"));
        let engaged = super::TieredFold {
            tier_read_fault_seeds: 3,
            tier_read_faults_fired: 1,
            ..Default::default()
        };
        assert!(engaged.shard_end_clauses().is_empty());
        assert!(super::TieredFold::default().shard_end_clauses().is_empty());
    }
}
