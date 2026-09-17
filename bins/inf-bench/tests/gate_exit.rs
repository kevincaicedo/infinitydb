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
        self.command_for("m0", "0", "1")
    }

    fn command_for(&self, milestone: &str, duration: &str, cells: &str) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_inf-bench"));
        command.args([
            "gate-run",
            milestone,
            "--unsafe-env",
            "--allow-dirty",
            "--cells",
            cells,
            "--duration",
            duration,
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
fn zero_duration_cannot_certify_a_generator_even_when_numeric_gates_pass() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    for (name, source, informational) in [
        ("complete", "tripwire:loop_iter_p999_us", false),
        ("informational", "external:unmeasured", true),
    ] {
        let fixture = Fixture::new(name, source, informational);
        let output = fixture.command().arg("--skip-fill").output().unwrap();
        fixture.assert_failed(&output, "generator saturation:");
        assert!(fixture.report().contains("UNMEASURED"));
        for row in [
            "m0 pipelined",
            "m0 routing natural",
            "m0 routing all-local",
            "m0 comparator infinityd",
            "m0 comparator Dragonfly",
            "m0 unpipelined infinityd",
            "m0 unpipelined Redis",
        ] {
            assert!(fixture.report().contains(&format!("| {row} | UNMEASURED |")));
        }
    }
}

#[test]
fn m2_only_always_probes_and_enforces_measured_dispositions() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    for (mode, expected, valid) in [
        ("plateau", "PLATEAU", true),
        ("limited", "GENERATOR-LIMITED", false),
        ("error", "UNMEASURED", false),
        ("disconnect", "UNMEASURED", false),
    ] {
        let fixture = Fixture::new(mode, "tripwire:spawn_retries", false);
        let output = fixture
            .command_for("m2", "3", "1")
            .arg("--only-always")
            .arg("--data-root")
            .arg(&fixture.0)
            .env("INF_GATE_TEST_PROBE", mode)
            .output()
            .unwrap();
        let report = fixture.report();
        assert_eq!(output.status.success(), valid, "{report}\n{:?}", output);
        assert!(report.contains(&format!("| m2 always grouped writes | {expected} |")), "{report}");
        assert!(report.contains("64 -> 96"));
        assert!(report.contains("generator m2 always grouped writes baseline"));
        assert!(report.contains("always row: 100 gated acks / 10 fsyncs"), "{report}");
        assert_eq!(report.contains("status: COMPLETE"), valid);
        if !valid {
            fixture.assert_failed(&output, "generator saturation:");
        }
    }
}

#[test]
fn m1_binary_counts_a_four_cell_node_total_once() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let fixture = Fixture::new("m1-memory", "external:unmeasured", true);
    let output = fixture
        .command_for("m1", "0", "4")
        .args([
            "--storm-keys",
            "1",
            "--flushall-keys",
            "1",
            "--maxmemory-mb",
            "16",
            "--subs",
            "1",
            "--sub-channels",
            "1",
            "--skip-fill",
        ])
        .output()
        .unwrap();
    fixture.assert_failed(&output, "generator saturation:");
    let report = fixture.report();
    assert!(report.contains("logical 508 B vs limit 16777216 B"), "{report}");
    assert!(report.contains("resident incl. slack/buffers: 4096 B"), "{report}");
    for row in ["m1 baseline", "m1 TTL-heavy", "m1 eviction pressure", "m1 KV under pubsub"] {
        assert!(report.contains(&format!("| {row} |")), "{report}");
    }
}

#[test]
fn m2_only_everysec_requires_both_namespace_probes() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let fixture = Fixture::new("m2-everysec", "external:unmeasured", true);
    let output = fixture
        .command_for("m2", "0", "1")
        .arg("--only-everysec")
        .arg("--data-root")
        .arg(&fixture.0)
        .output()
        .unwrap();
    fixture.assert_failed(&output, "generator saturation:");
    let report = fixture.report();
    for name in ["m2 everysec memory arm", "m2 everysec durable arm"] {
        assert!(report.contains(&format!("| {name} | UNMEASURED |")), "{report}");
    }
}

#[test]
fn m2_full_flow_disposes_memory_durable_and_checkpoint_arms() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let fixture = Fixture::new("m2-full", "external:unmeasured", true);
    let output = fixture
        .command_for("m2", "0", "1")
        .args(["--pressure-replicates", "1", "--attribution-keys", "1"])
        .arg("--baseline-bin")
        .arg(fixture.0.join("server"))
        .arg("--data-root")
        .arg(&fixture.0)
        .output()
        .unwrap();
    fixture.assert_failed(&output, "generator saturation:");
    let report = fixture.report();
    for row in [
        "pipelined 1:10 (M0 gate mix)",
        "unpipelined 512-conn (M0 gate mix)",
        "ttl-heavy 1:1 writes (M1 gate mix)",
    ] {
        for arm in ["m2", "m1-baseline"] {
            assert!(report.contains(&format!("| m2 {row} {arm} | UNMEASURED |")), "{report}");
        }
    }
    for row in [
        "m2 always grouped writes",
        "m2 everysec memory arm",
        "m2 everysec durable arm",
        "m2 ckpt-pressure baseline rep 0",
        "m2 ckpt-pressure pressure rep 0",
    ] {
        assert!(report.contains(&format!("| {row} | UNMEASURED |")), "{report}");
    }
}
