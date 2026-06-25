use std::collections::BTreeSet;

use inf_log::fault::M2_DURABILITY_FAULT_POINTS;
use toml::Value;

const MATRIX_TOML: &str = include_str!("../../../tests/crash-matrix/m2.toml");

#[test]
fn crash_matrix_definition_lists_every_m2_fault_point() {
    let matrix = parse_matrix();
    let declared = string_array(&matrix, "fault_points");
    let expected = fault_point_names();

    assert_eq!(declared, expected);
    assert_unique(&declared);
}

#[test]
fn crash_matrix_axes_define_the_m2_cartesian_space() {
    let matrix = parse_matrix();
    assert_eq!(required_string(&matrix, "schema"), "infinitydb.m2.crash-matrix.v1");
    assert_eq!(required_string(&matrix, "milestone"), "M2-S17");
    assert_eq!(required_string(&matrix, "status"), "partial-runner");

    let policies = string_array(&matrix, "fsync_policies");
    assert_eq!(policies, vec!["everysec", "always"]);
    assert_unique(&policies);

    let workloads = workload_names(&matrix);
    assert_eq!(
        workloads,
        vec![
            "single_write",
            "batched_pipeline",
            "restart_watermark",
            "segment_recovery",
            "checkpoint_tail",
            "live_checkpoint_command",
            "manifest_replacement",
            "public_recovered_state_sweep",
            "public_everysec_workload_sweep",
        ]
    );
    assert_unique(&workloads);

    let row_count = fault_point_names().len() * policies.len() * workloads.len();
    assert_eq!(row_count, 144);
}

#[test]
fn crash_matrix_fault_coverage_is_complete_and_honest() {
    let matrix = parse_matrix();
    let coverage = required_array(&matrix, "fault_coverage");
    let known: BTreeSet<_> = fault_point_names().into_iter().collect();
    let mut declared = BTreeSet::new();
    let mut covered = BTreeSet::new();

    for row in coverage {
        let point = row_string(row, "fault_point");
        assert!(known.contains(point), "unknown fault point in coverage row: {point}");
        assert!(declared.insert(point), "duplicate coverage row for {point}");

        match row_string(row, "status") {
            "unit-covered" => {
                assert!(!row_string(row, "test").is_empty());
                assert!(!row_string(row, "notes").is_empty());
                covered.insert(point);
            }
            "runner-covered" => {
                assert!(!row_string(row, "test").is_empty());
                assert!(!row_string(row, "notes").is_empty());
                covered.insert(point);
            }
            "pending-real-path" => {
                assert!(!row_string(row, "blocked_by").is_empty());
                assert!(!row_string(row, "notes").is_empty());
            }
            status => panic!("unsupported coverage status {status} for {point}"),
        }
    }

    assert_eq!(declared, known);
    assert_eq!(
        covered,
        BTreeSet::from([
            "dir_fsync_fail",
            "fsync_err",
            "log_append_short_write",
            "manifest_rename_fail",
            "power_cut_after_manifest",
            "power_cut_after_seal",
            "checkpoint_write_enospc",
            "torn_frame"
        ])
    );
}

#[test]
fn crash_matrix_tracks_s14_active_tail_recovery_paths() {
    let matrix = parse_matrix();
    let coverage = required_array(&matrix, "recovery_path_coverage");
    let expected = BTreeSet::from(["active_tail_later_magic", "torn_final_frame"]);
    let mut declared = BTreeSet::new();

    for row in coverage {
        let path = row_string(row, "path");
        assert!(expected.contains(path), "unknown recovery path {path}");
        assert!(declared.insert(path), "duplicate recovery path {path}");
        assert_eq!(row_string(row, "fault_point"), "torn_frame");
        assert_eq!(row_string(row, "workload"), "segment_recovery");
        assert_eq!(row_string(row, "status"), "runner-covered");
        assert!(!row_string(row, "test").is_empty());
        assert!(!row_string(row, "oracle").is_empty());
        assert!(!row_string(row, "notes").is_empty());
    }

    assert_eq!(declared, expected);
}

#[test]
fn crash_matrix_runner_rows_are_axis_valid_and_honest() {
    let matrix = parse_matrix();
    let rows = required_array(&matrix, "runner_rows");
    let fault_points: BTreeSet<_> = fault_point_names().into_iter().collect();
    let policies: BTreeSet<_> = string_array(&matrix, "fsync_policies").into_iter().collect();
    let workloads: BTreeSet<_> = workload_names(&matrix).into_iter().collect();
    let expected = BTreeSet::from([
        "public_always_single_write_power_cut",
        "public_always_batched_pipeline_power_cut",
        "public_everysec_single_write_contract",
        "public_always_single_write_fsync_err_fail_stop",
        "public_fsync_err_after_prior_frame_recovers_previous_watermark",
        "public_log_append_write_fault_fail_stop",
        "public_power_cut_after_seal_recovers_rotated_segment",
        "public_power_cut_after_non_exact_seal_recovers_truncated_segment",
        "public_torn_final_frame_recovers_stable_prefix",
        "public_active_tail_later_magic_truncates_prefix",
        "public_manifest_checkpoint_tail_power_cut",
        "public_manifest_rename_fail_full_log_recovery",
        "public_manifest_dir_fsync_fail_full_log_recovery",
        "public_live_checkpoint_wait_dir_fsync_fail_no_reply",
        "public_checkpoint_write_enospc_preserves_old_manifest",
        "public_manifest_replacement_dir_fsync_fail_preserves_old_manifest",
        "public_manifest_replacement_rename_fail_preserves_old_manifest",
        "public_always_recovered_state_sweep",
        "public_everysec_loss_window_sweep",
        "public_everysec_workload_sweep",
    ]);
    let mut declared = BTreeSet::new();

    for row in rows {
        let id = row_string(row, "id");
        assert!(expected.contains(id), "unknown runner row {id}");
        assert!(declared.insert(id), "duplicate runner row {id}");
        assert!(fault_points.contains(row_string(row, "fault_point")));
        assert!(policies.contains(row_string(row, "fsync_policy")));
        assert!(workloads.contains(row_string(row, "workload")));
        assert_eq!(row_string(row, "status"), "ci-green");
        assert!(!row_string(row, "seed").is_empty());
        assert!(!row_string(row, "test").is_empty());
        assert_eq!(
            row_string(row, "runner"),
            "inf-sim --scenario m2-crash-matrix --verify-determinism"
        );
        if id == "public_always_single_write_fsync_err_fail_stop" {
            assert_eq!(
                row_string(row, "process_test"),
                "inf-sim::m2_crash_matrix::m2_fsync_err_process_fail_stop_exits_nonzero"
            );
            assert_eq!(
                row_string(row, "process_runner"),
                "inf-sim --scenario m2-fsync-err-process-fail-stop --seed 0xF5E10023"
            );
            assert!(!row_string(row, "process_oracle").is_empty());
        } else if id == "public_log_append_write_fault_fail_stop" {
            assert_eq!(
                row_string(row, "process_test"),
                "inf-sim::m2_crash_matrix::m2_log_append_write_fault_process_fail_stop_exits_nonzero"
            );
            assert_eq!(
                row_string(row, "process_runner"),
                "inf-sim --scenario m2-log-append-write-fault-process-fail-stop --seed 0xD279"
            );
            assert!(!row_string(row, "process_oracle").is_empty());
        } else if id == "public_live_checkpoint_wait_dir_fsync_fail_no_reply" {
            assert_eq!(
                row_string(row, "process_test"),
                "inf-sim::m2_crash_matrix::m2_live_checkpoint_dir_fsync_process_fail_stop_exits_nonzero"
            );
            assert_eq!(
                row_string(row, "process_runner"),
                "inf-sim --scenario m2-live-checkpoint-dir-fsync-process-fail-stop --seed 0xD290"
            );
            assert!(!row_string(row, "process_oracle").is_empty());
        } else {
            assert!(row.get("process_test").is_none(), "{id} has unexpected process_test");
            assert!(row.get("process_runner").is_none(), "{id} has unexpected process_runner");
            assert!(row.get("process_oracle").is_none(), "{id} has unexpected process_oracle");
        }
        assert!(!row_string(row, "oracle").is_empty());
        assert!(!row_string(row, "notes").is_empty());
    }

    assert_eq!(declared, expected);
}

fn parse_matrix() -> Value {
    toml::from_str::<Value>(MATRIX_TOML).expect("m2 crash matrix TOML parses")
}

fn fault_point_names() -> Vec<&'static str> {
    M2_DURABILITY_FAULT_POINTS.iter().map(|point| point.name()).collect()
}

fn required_string<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or_else(|| panic!("missing string {key}"))
}

fn string_array<'a>(value: &'a Value, key: &str) -> Vec<&'a str> {
    required_array(value, key)
        .iter()
        .map(|entry| entry.as_str().unwrap_or_else(|| panic!("{key} contains a non-string")))
        .collect()
}

fn workload_names(value: &Value) -> Vec<&str> {
    required_array(value, "workload_shapes").iter().map(|entry| row_string(entry, "name")).collect()
}

fn required_array<'a>(value: &'a Value, key: &str) -> &'a Vec<Value> {
    value.get(key).and_then(Value::as_array).unwrap_or_else(|| panic!("missing array {key}"))
}

fn row_string<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or_else(|| panic!("missing row string {key}"))
}

fn assert_unique(names: &[&str]) {
    let mut seen = BTreeSet::new();
    for name in names {
        assert!(seen.insert(*name), "duplicate name {name}");
    }
}
