//! M2-S17 process-boundary crash-matrix checks.

use std::process::Command;

#[test]
fn m2_fsync_err_process_fail_stop_exits_nonzero() {
    let output = Command::new(env!("CARGO_BIN_EXE_inf-sim"))
        .args(["--scenario", "m2-fsync-err-process-fail-stop", "--seed", "0xF5E10023"])
        .output()
        .expect("run inf-sim fsync_err process scenario");

    assert!(
        !output.status.success(),
        "fsync_err process scenario unexpectedly succeeded: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_ne!(
        output.status.code(),
        Some(2),
        "scenario was rejected by CLI parsing: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("fdatasync after frame"), "stderr={stderr}");
    assert!(stderr.contains("errno 5"), "stderr={stderr}");
    assert!(!stderr.contains("reply bytes before fail-stop"), "stderr={stderr}");
}

#[test]
fn m2_log_append_write_fault_process_fail_stop_exits_nonzero() {
    let output = Command::new(env!("CARGO_BIN_EXE_inf-sim"))
        .args(["--scenario", "m2-log-append-write-fault-process-fail-stop", "--seed", "0xD279"])
        .output()
        .expect("run inf-sim log append write-fault process scenario");

    assert!(
        !output.status.success(),
        "log append write-fault process scenario unexpectedly succeeded: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_ne!(
        output.status.code(),
        Some(2),
        "scenario was rejected by CLI parsing: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("file write"), "stderr={stderr}");
    assert!(stderr.contains("errno 5"), "stderr={stderr}");
    assert!(!stderr.contains("reply bytes before fail-stop"), "stderr={stderr}");
}

#[test]
fn m2_live_checkpoint_dir_fsync_process_fail_stop_exits_nonzero() {
    let output = Command::new(env!("CARGO_BIN_EXE_inf-sim"))
        .args(["--scenario", "m2-live-checkpoint-dir-fsync-process-fail-stop", "--seed", "0xD290"])
        .output()
        .expect("run inf-sim live checkpoint dir-fsync process scenario");

    assert!(
        !output.status.success(),
        "live checkpoint process scenario unexpectedly succeeded: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_ne!(
        output.status.code(),
        Some(2),
        "scenario was rejected by CLI parsing: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("checkpoint directory"), "stderr={stderr}");
    assert!(stderr.contains("errno 5"), "stderr={stderr}");
    assert!(!stderr.contains("reply bytes before fail-stop"), "stderr={stderr}");
}
