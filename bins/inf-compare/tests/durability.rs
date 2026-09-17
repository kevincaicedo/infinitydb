//! L20-19: unsupported durable runs fail before any engine or data is touched.
use std::process::Command;

#[test]
fn dragonfly_everysec_is_refused_before_launch_or_artifacts() {
    let root = std::env::temp_dir().join(format!("inf-compare-durability-{}", std::process::id()));
    std::fs::create_dir_all(root.join("data/dragonfly")).unwrap();
    let sentinel = root.join("data/dragonfly/keep");
    std::fs::write(&sentinel, b"existing data").unwrap();
    for placement in [vec![], vec!["--docker"], vec!["--attach", "dragonfly=127.0.0.1:1"]] {
        let output = Command::new(env!("CARGO_BIN_EXE_inf-compare"))
            .env("PATH", root.join("empty-path"))
            .args([
                "run",
                "--engines",
                "dragonfly",
                "--durability",
                "everysec",
                "--workload",
                "set",
                "--pipeline",
                "1",
            ])
            .args(placement)
            .arg("--data-root")
            .arg(root.join("data"))
            .arg("--out")
            .arg(root.join("out"))
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{stderr}");
        assert!(stderr.contains("dragonfly does not support --durability everysec"), "{stderr}");
        assert!(stderr.contains("--engines redis,infinitydb"), "{stderr}");
        assert!(!root.join("out").exists(), "refused before creating artifacts");
        assert_eq!(std::fs::read(&sentinel).unwrap(), b"existing data");
    }
    std::fs::remove_dir_all(root).unwrap();
}
