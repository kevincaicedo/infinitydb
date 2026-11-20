#![allow(
    clippy::disallowed_methods,
    reason = "test-only: filesystem fixtures outside cell code (ADR-0144 D5)"
)]
//! L20-21: reference admission requires the authoritative checker.
#![cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

#[test]
fn missing_unexecutable_and_failed_checkers_refuse_before_launch_or_artifacts() {
    let root = std::env::temp_dir().join(format!("inf-compare-reference-{}", std::process::id()));
    // The comparator reads its checker from the target root that built it, the
    // siblings of its own directory, so a copy of it in a fixture root reads
    // the fixture's checker, never a real one.
    let bin = root.join("target/release/inf-compare");
    let checker = root.join("target/release/inf-bench");
    std::fs::create_dir_all(checker.parent().unwrap()).unwrap();
    let copied = Command::new("cp").arg(env!("CARGO_BIN_EXE_inf-compare")).arg(&bin).status();
    assert!(copied.unwrap().success(), "copy the comparator into {}", root.display());
    for (mode, reason) in
        [(None, "unavailable"), (Some(0o600), "unavailable"), (Some(0o700), "failed")]
    {
        if let Some(mode) = mode {
            std::fs::write(&checker, "#!/bin/sh\nexit 7\n").unwrap();
            std::fs::set_permissions(&checker, std::fs::Permissions::from_mode(mode)).unwrap();
        }
        let output = Command::new(&bin)
            .current_dir(&root)
            .args(["run", "--reference-box", "--engines", "redis", "--workload", "set", "--out"])
            .arg(root.join("out"))
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(1), "{stderr}");
        assert!(stderr.contains(&format!("`inf-bench env-check` {reason}")), "{stderr}");
        assert!(stderr.contains("refusing a binding run"), "{stderr}");
        assert!(!root.join("out").exists());
    }
    std::fs::remove_dir_all(root).unwrap();
}
