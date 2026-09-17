//! Exercise CLI verdicts with short workloads and synthetic server fixtures.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output};

#[allow(clippy::disallowed_types, reason = "serialize high-connection CLI fixtures in this test")]
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn zero_replicates_refuse_before_running_any_milestone() {
    for milestone in ["m0", "m1", "m2", "m4", "m4.5"] {
        let output = Command::new(env!("CARGO_BIN_EXE_inf-bench"))
            .args(["gate-run", milestone, "--replicates", "0"])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1));
        assert!(String::from_utf8_lossy(&output.stderr).contains("--replicates must be >= 1"));
    }
}

struct Fixture(PathBuf);

impl Fixture {
    fn new(name: &str, source: &str, informational: bool) -> Self {
        let root =
            std::env::temp_dir().join(format!("inf-bench-gate-{}-{name}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        for name in ["server", "redis", "dragonfly"] {
            let path = root.join(name);
            std::fs::write(&path, include_str!("fixtures/gate_server.py")).unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        std::fs::write(
            root.join("gates.toml"),
            format!(
                "[[gate]]\nid = \"probe\"\nname = \"probe\"\nthreshold = 1\n\
             comparator = \"<=\"\n\
             tier = \"linux-reference-box\"\nsource = \"{source}\"\n\
             informational = {informational}\n"
            ),
        )
        .unwrap();
        Self(root)
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_inf-bench"));
        command.args([
            "gate-run",
            "m0",
            "--unsafe-env",
            "--allow-dirty",
            "--cells",
            "1",
            "--duration",
            "0",
            "--replicates",
            "1",
            "--fill-keys",
            "0",
        ]);
        for (flag, path) in [
            ("--gates", "gates.toml"),
            ("--infinityd-bin", "server"),
            ("--dragonfly-bin", "dragonfly"),
            ("--redis-bin", "redis"),
            ("--artifacts-root", "out"),
        ] {
            command.arg(flag).arg(self.0.join(path));
        }
        command
    }

    fn report(&self) -> String {
        let dir = std::fs::read_dir(self.0.join("out")).unwrap().next().unwrap().unwrap();
        std::fs::read_to_string(dir.path().join("report.md")).unwrap()
    }

    fn assert_failed(&self, output: &Output, reason: &str) {
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(1), "{stderr}");
        assert!(stderr.contains(reason), "{stderr}");
        let report = self.report();
        assert!(report.contains("NOT citation-grade"), "{report}");
        assert!(report.contains(reason), "{report}");
        assert!(!report.contains("status: COMPLETE"), "{report}");
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn unmeasured_stop_exits_one_with_an_incomplete_report() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let fixture = Fixture::new("missing", "external:unmeasured", false);
    let output = fixture.command().arg("--skip-fill").output().unwrap();
    fixture.assert_failed(&output, "unmeasured STOP gate(s): probe (external:unmeasured)");
}

#[test]
fn missing_dragonfly_exits_one_even_without_a_comparator_gate() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let fixture = Fixture::new("dragonfly", "tripwire:loop_iter_p999_us", false);
    std::fs::remove_file(fixture.0.join("dragonfly")).unwrap();
    let output = fixture.command().arg("--skip-fill").output().unwrap();
    fixture.assert_failed(&output, "Dragonfly comparator startup failed");
}

#[test]
fn missing_redis_exits_one_even_without_a_redis_gate() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let fixture = Fixture::new("redis", "tripwire:loop_iter_p999_us", false);
    std::fs::remove_file(fixture.0.join("redis")).unwrap();
    let output = fixture.command().arg("--skip-fill").output().unwrap();
    fixture.assert_failed(&output, "Redis A/B startup failed");
}

#[test]
fn redis_rss_startup_failure_is_fatal_after_successful_ab_startup() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let fixture = Fixture::new("redis-rss", "tripwire:loop_iter_p999_us", false);
    let output = fixture.command().env("INF_GATE_TEST_DISABLE_REDIS", "1").output().unwrap();
    fixture.assert_failed(&output, "Redis RSS startup failed");
    assert!(!fixture.report().contains("Redis A/B startup failed"));
}

#[test]
fn complete_run_and_missing_informational_measurement_exit_zero() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    for (name, source, informational) in [
        ("complete", "tripwire:loop_iter_p999_us", false),
        ("informational", "external:unmeasured", true),
    ] {
        let fixture = Fixture::new(name, source, informational);
        let output = fixture.command().arg("--skip-fill").output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        assert!(fixture.report().contains("status: COMPLETE"));
    }
}
