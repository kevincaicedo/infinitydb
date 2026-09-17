//! L20-21: reference admission requires the authoritative checker.
#![cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

#[test]
fn missing_unexecutable_and_failed_checkers_refuse_before_launch_or_artifacts() {
    let root = std::env::temp_dir().join(format!("inf-compare-reference-{}", std::process::id()));
    let checker = root.join("target/release/inf-bench");
    std::fs::create_dir_all(checker.parent().unwrap()).unwrap();
    for (mode, reason) in
        [(None, "unavailable"), (Some(0o600), "unavailable"), (Some(0o700), "failed")]
    {
        if let Some(mode) = mode {
            std::fs::write(&checker, "#!/bin/sh\nexit 7\n").unwrap();
            std::fs::set_permissions(&checker, std::fs::Permissions::from_mode(mode)).unwrap();
        }
        let output = Command::new(env!("CARGO_BIN_EXE_inf-compare"))
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
