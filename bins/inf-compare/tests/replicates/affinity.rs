//! Binary witnesses share the orchestration fixtures with the replicate tests.
#![cfg(target_os = "linux")]

use super::*;

fn available_cpus() -> Vec<usize> {
    let output = Command::new("/usr/bin/python3")
        .args(["-c", "import os; print(*sorted(os.sched_getaffinity(0)))"])
        .output()
        .unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout)
        .unwrap()
        .split_whitespace()
        .map(|cpu| cpu.parse().unwrap())
        .collect()
}

#[test]
fn process_threads_children_and_all_generator_paths_observe_disjoint_masks() {
    let cpus = available_cpus();
    assert!(cpus.len() >= 2, "affinity fixture requires two available logical CPUs");
    let fixture = Fixture::new("cpu-placement");
    std::fs::write(fixture.0.join("observe-affinity"), "").unwrap();
    for name in ["redis-server", "dragonfly"] {
        std::fs::copy(fixture.0.join("infinityd"), fixture.0.join(name)).unwrap();
    }
    let server = cpus[0].to_string();
    let load = cpus[1].to_string();
    let output = fixture.run(&[
        "--engines",
        "redis,dragonfly,infinitydb",
        "--generator",
        "both",
        "--workload",
        "set,get,memory",
        "--pipeline",
        "1",
        "--replicates",
        "1",
        "--threads",
        "1",
        "--pin-start",
        &server,
        "--load-pin-start",
        &load,
        "--port-base",
        &host_port_block().to_string(),
    ]);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let observations = std::fs::read_to_string(fixture.0.join("affinity.tsv")).unwrap();
    for line in observations.lines() {
        let fields: Vec<_> = line.split('\t').collect();
        let expected = if fields[0].starts_with("server-") { &server } else { &load };
        assert_eq!(fields[2], expected, "{line}");
    }
    for role in [
        "server-infinityd",
        "server-redis-server",
        "server-dragonfly",
        "memtier-fill",
        "memtier-measured",
        "redis-benchmark",
    ] {
        for stage in ["main", "thread", "child"] {
            assert!(observations.contains(&format!("{role}\t{stage}\t")), "{observations}");
        }
    }
    let report = std::fs::read_to_string(fixture.run_dir().join("report.md")).unwrap();
    assert!(report.contains(&format!("server CPUs {server}")), "{report}");
    assert!(report.contains(&format!("generator CPUs {load}")), "{report}");
    assert!(!report.contains("taskset from util-linux"), "wrapper version is not engine version");
    assert!(report.contains("infinityd fixture"), "{report}");
    let placement = std::fs::read_to_string(fixture.run_dir().join("placement.txt")).unwrap();
    assert!(placement.contains(&format!("generator CPUs {load}")));
}

#[test]
fn invalid_or_uncontrolled_cpu_placement_refuses_before_launch_and_artifacts() {
    let fixture = Fixture::new("invalid-cpu-placement");
    let maximum = usize::MAX.to_string();
    let cases: &[(&[&str], &str)] = &[
        (&["--threads", "0"], "--threads must be greater than zero"),
        (&["--load-cpus", "0"], "--load-cpus must be greater than zero"),
        (&["--pin-start", "0"], "requires both"),
        (&["--load-pin-start", "4"], "requires both"),
        (&["--load-cpus", "1"], "requires both"),
        (&["--reference-box", "--unsafe-env"], "requires both"),
        (&["--pin-start", "0", "--load-pin-start", "3"], "must be disjoint"),
        (&["--pin-start", &maximum, "--load-pin-start", "0"], "endpoint overflow"),
        (&["--docker", "--pin-start", "0", "--load-pin-start", "4"], "Linux host launches"),
        (
            &["--attach", "infinitydb=127.0.0.1:1", "--pin-start", "0", "--load-pin-start", "4"],
            "Linux host launches",
        ),
    ];
    for (args, reason) in cases {
        let mut options = vec!["--engines", "infinitydb", "--workload", "set"];
        options.extend_from_slice(args);
        let output = fixture.run(&options);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{args:?}");
        assert!(stderr.contains(reason), "{args:?}: {stderr}");
        assert!(!fixture.0.join("launches").exists());
        assert!(!fixture.0.join("out").exists());
    }
}

#[test]
fn unavailable_failing_and_trimmed_taskset_cannot_silently_disable_pinning() {
    let fixture = Fixture::new("failed-taskset");
    let taskset = fixture.0.join("taskset");
    let absent = Command::new(env!("CARGO_BIN_EXE_inf-compare"))
        .current_dir(&fixture.0)
        .env("PATH", "")
        .args([
            "run",
            "--out",
            "out",
            "--engines",
            "infinitydb",
            "--workload",
            "set",
            "--threads",
            "2",
            "--pin-start",
            "0",
            "--load-pin-start",
            "2",
        ])
        .output()
        .unwrap();
    assert!(!absent.status.success());
    assert!(String::from_utf8_lossy(&absent.stderr).contains("verify CPU range"));
    assert!(!fixture.0.join("out").exists());
    for (source, reason) in [
        ("#!/bin/sh\nexit 7\n", "taskset exited"),
        ("#!/bin/sh\nprintf 'Cpus_allowed_list:\\t0\\n'\n", "not fully applied"),
    ] {
        std::fs::write(&taskset, source).unwrap();
        std::fs::set_permissions(&taskset, std::fs::Permissions::from_mode(0o700)).unwrap();
        let output = fixture.run(&[
            "--engines",
            "infinitydb",
            "--workload",
            "set",
            "--threads",
            "2",
            "--pin-start",
            "0",
            "--load-pin-start",
            "2",
        ]);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains(reason));
        assert!(!fixture.0.join("launches").exists());
        assert!(!fixture.0.join("out").exists());
    }
}
