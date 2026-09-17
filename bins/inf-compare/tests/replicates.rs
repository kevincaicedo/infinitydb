//! Exercise the shipping orchestrator with deterministic servers and generators.
#![cfg(unix)]

#[path = "replicates/affinity.rs"]
mod affinity_cases;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};

static FIXTURE_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static HOST_PORT: AtomicU16 = AtomicU16::new(0);

struct Server {
    port: u16,
    stopped: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let stopped = Arc::new(AtomicBool::new(false));
        let done = stopped.clone();
        let thread = std::thread::spawn(move || {
            for stream in listener.incoming() {
                if done.load(Ordering::Relaxed) {
                    break;
                }
                let mut stream = stream.unwrap();
                stream.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
                let mut reader = BufReader::new(&stream);
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let count: usize = line.trim().strip_prefix('*').unwrap().parse().unwrap();
                let mut args = Vec::new();
                for _ in 0..count {
                    line.clear();
                    reader.read_line(&mut line).unwrap();
                    let size: usize = line.trim().strip_prefix('$').unwrap().parse().unwrap();
                    let mut bytes = vec![0; size + 2];
                    reader.read_exact(&mut bytes).unwrap();
                    args.push(bytes[..size].to_vec());
                }
                let reply = match args[0].as_slice() {
                    b"PING" => b"+PONG\r\n".to_vec(),
                    b"DBSIZE" => b":100\r\n".to_vec(),
                    b"INFO" => {
                        let text = "redis_version:fixture\r\nused_cpu_user:0\r\nused_cpu_sys:0\r\n";
                        format!("${}\r\n{text}\r\n", text.len()).into_bytes()
                    }
                    _ => b"+OK\r\n".to_vec(),
                };
                stream.write_all(&reply).unwrap();
            }
        });
        Self { port, stopped, thread: Some(thread) }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        self.thread.take().unwrap().join().unwrap();
    }
}

struct Fixture(PathBuf);

impl Fixture {
    fn new(name: &str) -> Self {
        let sequence = FIXTURE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("compare-{name}-{}-{sequence}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let generator = root.join("memtier_benchmark");
        std::fs::write(&generator, include_str!("fixtures/memtier.sh")).unwrap();
        std::fs::set_permissions(generator, std::fs::Permissions::from_mode(0o700)).unwrap();
        let engine = root.join("infinityd");
        std::fs::write(&engine, include_str!("fixtures/infinityd.py")).unwrap();
        std::fs::set_permissions(engine, std::fs::Permissions::from_mode(0o700)).unwrap();
        let generator = root.join("redis-benchmark");
        std::fs::write(&generator, include_str!("fixtures/redisbench.sh")).unwrap();
        std::fs::set_permissions(generator, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(root.join("affinity.py"), include_str!("fixtures/affinity.py")).unwrap();
        Self(root)
    }

    fn run(&self, args: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_inf-compare"));
        command
            .current_dir(&self.0)
            .env("PATH", format!("{}:/usr/bin:/bin", self.0.display()))
            .env("COMPARE_FIXTURE", &self.0)
            .args(["run", "--out", "out"]);
        if !args.contains(&"--generator") {
            command.args(["--generator", "memtier"]);
        }
        command.args(args).output().unwrap()
    }

    fn run_dir(&self) -> PathBuf {
        std::fs::read_dir(self.0.join("out")).unwrap().next().unwrap().unwrap().path()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!("fixture evidence retained at {}", self.0.display());
        } else {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }
}

fn host_port_block() -> u16 {
    // Stay below ephemeral clients; the largest fixture campaign needs nine ports.
    loop {
        let base = 20_000
            + u16::try_from(std::process::id() % 8_000).unwrap()
            + HOST_PORT.fetch_add(16, Ordering::Relaxed);
        assert!(base < 30_000, "host fixture port range exhausted");
        let probes: Result<Vec<_>, _> =
            (base..base + 16).map(|port| TcpListener::bind(("127.0.0.1", port))).collect();
        if probes.is_ok() {
            return base;
        }
    }
}

fn files_with_extension(root: &Path, extension: &str) -> usize {
    std::fs::read_dir(root)
        .unwrap()
        .map(|entry| {
            let path = entry.unwrap().path();
            if path.is_dir() {
                files_with_extension(&path, extension)
            } else {
                usize::from(path.extension().is_some_and(|value| value == extension))
            }
        })
        .sum()
}

#[test]
fn default_replicates_rotate_each_workload_pipeline_and_preserve_samples() {
    let fixture = Fixture::new("default-replicates");
    let servers = [Server::start(), Server::start(), Server::start()];
    let attach = format!(
        "redis=127.0.0.1:{},dragonfly=127.0.0.1:{},infinitydb=127.0.0.1:{}",
        servers[0].port, servers[1].port, servers[2].port
    );
    let output = fixture.run(&[
        "--engines",
        "redis,dragonfly,infinitydb",
        "--attach",
        &attach,
        "--workload",
        "set,get",
        "--pipeline",
        "1,4",
    ]);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let events = std::fs::read_to_string(fixture.0.join("events")).unwrap();
    let ports: Vec<u16> = events.lines().map(|line| line.parse().unwrap()).collect();
    let rotation = [0, 1, 2, 1, 2, 0, 2, 0, 1].map(|index| servers[index].port);
    assert_eq!(ports, rotation.repeat(4), "each scenario needs three rotated rounds");
    let run = fixture.run_dir();
    assert_eq!(files_with_extension(&run.join("raw"), "json"), 36);
    let report = std::fs::read_to_string(run.join("report.md")).unwrap();
    assert!(report.contains("| Replicates | 3 |"), "{report}");
    assert!(report.contains("Relative spread (%)"), "{report}");
    assert!(report.contains("| 3 | 20.000 | 10.000 | 30.000 | 100.00 |"), "{report}");
    let summaries: Vec<_> =
        report.lines().filter(|line| line.contains("| memtier | ops/s |")).collect();
    assert_eq!(summaries.len(), 12);
    for row in summaries {
        assert!(row.ends_with("| 3 | 20.000 | 10.000 | 30.000 | 100.00 |"), "{row}");
    }
    let manifest = std::fs::read_to_string(run.join("schedule.tsv")).unwrap();
    assert_eq!(manifest.lines().filter(|line| line.ends_with("\tcomplete")).count(), 36);
}

#[test]
fn invalid_counts_and_short_reference_runs_refuse_before_artifacts() {
    let fixture = Fixture::new("invalid-replicates");
    for count in ["0", "6", "-1", "nope"] {
        let output = fixture.run(&["--engines", "redis", "--replicates", count]);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("replicates"));
        assert!(!fixture.0.join("out").exists());
    }
    for count in ["1", "2"] {
        let output = fixture.run(&[
            "--engines",
            "redis",
            "--replicates",
            count,
            "--reference-box",
            "--unsafe-env",
        ]);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success());
        assert!(stderr.contains("--reference-box requires 3–5 replicates"), "{stderr}");
        assert!(!fixture.0.join("out").exists());
    }
}

#[test]
fn explicit_counts_reverse_two_engines_and_keep_both_generators() {
    let servers = [Server::start(), Server::start()];
    let attach =
        format!("infinitydb=127.0.0.1:{},redis=127.0.0.1:{}", servers[0].port, servers[1].port);
    for count in 1..=5 {
        let fixture = Fixture::new(&format!("explicit-{count}"));
        let output = fixture.run(&[
            "--engines",
            "infinitydb,redis",
            "--attach",
            &attach,
            "--replicates",
            &count.to_string(),
            "--workload",
            "set",
            "--pipeline",
            "1",
            "--generator",
            "both",
        ]);
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let events = std::fs::read_to_string(fixture.0.join("events")).unwrap();
        let ports: Vec<u16> = events.lines().map(|line| line.parse().unwrap()).collect();
        let expected: Vec<u16> = (0..count)
            .flat_map(|round| [round % 2, (round + 1) % 2])
            .map(|index| servers[index].port)
            .collect();
        assert_eq!(ports, expected);
        let run = fixture.run_dir();
        assert_eq!(files_with_extension(&run.join("raw"), "json"), 2 * count);
        assert_eq!(files_with_extension(&run.join("raw"), "csv"), 2 * count);
        let report = std::fs::read_to_string(run.join("report.md")).unwrap();
        assert!(report.contains(&format!("| redis-benchmark | req/s | {count} | 40.000 |")));
        assert_table_widths(&report);
    }
}

fn assert_table_widths(report: &str) {
    let mut expected = None;
    for line in report.lines() {
        if !line.starts_with('|') {
            expected = None;
            continue;
        }
        let count = line.matches('|').count();
        let previous = expected.get_or_insert(count);
        assert_eq!(count, *previous, "malformed table row: {line}");
    }
}

#[test]
fn failed_leg_and_oversized_generator_file_cannot_produce_success_report() {
    let server = Server::start();
    let attach = format!("redis=127.0.0.1:{}", server.port);
    for (mode, reason) in [("fail", "exited"), ("oversize", "JSON byte limit")] {
        let fixture = Fixture::new(mode);
        std::fs::write(fixture.0.join(mode), "").unwrap();
        let output = fixture.run(&["--attach", &attach, "--workload", "set", "--pipeline", "1"]);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{stderr}");
        assert!(stderr.contains(reason), "{stderr}");
        let run = fixture.run_dir();
        assert!(!run.join("report.md").exists());
        let manifest = std::fs::read_to_string(run.join("schedule.tsv")).unwrap();
        assert!(manifest.lines().last().unwrap().ends_with("\tfailed"), "{manifest}");
        let complete = manifest.lines().filter(|line| line.ends_with("\tcomplete")).count();
        assert_eq!(complete, usize::from(mode == "fail"));
    }
}

#[test]
fn memory_has_one_sample_per_replicate_and_empty_selections_refuse() {
    let server = Server::start();
    let attach = format!("redis=127.0.0.1:{}", server.port);
    let fixture = Fixture::new("memory-replicates");
    let output = fixture.run(&["--attach", &attach, "--workload", "memory", "--pipeline", "1,4"]);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let run = fixture.run_dir();
    let report = std::fs::read_to_string(run.join("report.md")).unwrap();
    assert!(report.contains("| memory | keys | 3 | 100.000 |"), "{report}");
    assert_eq!(files_with_extension(&run.join("raw"), "tsv"), 3);
    assert_table_widths(&report);
    let fixture = Fixture::new("empty-selections");
    for args in [
        vec!["--engines", ""],
        vec!["--engines", "redis", "--pipeline", "0"],
        vec!["--engines", "redis", "--pipeline", "1,1"],
        vec!["--engines", "redis", "--workload", "set,set"],
        vec!["--engines", "redis,infinitydb", "--port-base", "65535"],
    ] {
        let output = fixture.run(&args);
        assert!(!output.status.success());
        assert!(!fixture.0.join("out").exists());
    }
}

#[test]
fn host_replicates_start_fresh_and_reap_children_on_success_and_setup_failure() {
    for mode in ["host-success", "bad-setup", "fail"] {
        let fixture = Fixture::new(mode);
        if mode != "host-success" {
            std::fs::write(fixture.0.join(mode), "").unwrap();
        }
        let port = host_port_block();
        let output = fixture.run(&[
            "--engines",
            "infinitydb",
            "--port-base",
            &port.to_string(),
            "--workload",
            "set",
            "--pipeline",
            "1",
            "--durability",
            "everysec",
            "--data-root",
            "data",
        ]);
        assert_eq!(
            output.status.success(),
            mode == "host-success",
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let launches = std::fs::read_to_string(fixture.0.join("launches")).unwrap();
        let expected = match mode {
            "host-success" => 3,
            "fail" => 2,
            _ => 1,
        };
        assert_eq!(launches.lines().count(), expected);
        let ports = std::fs::read_to_string(fixture.0.join("ports")).unwrap();
        for (index, actual) in ports.lines().enumerate() {
            let actual: u16 = actual.parse().unwrap();
            assert_eq!(actual, port + index as u16);
            assert!(TcpStream::connect(("127.0.0.1", actual)).is_err());
        }
        for process in launches.lines() {
            let status = Command::new("kill").args(["-0", process]).output().unwrap().status;
            assert!(!status.success(), "owned child {process} survived campaign exit");
        }
    }
}

#[test]
fn skipped_engines_do_not_bias_eligible_engine_order() {
    let fixture = Fixture::new("skipped-order");
    let servers = [Server::start(), Server::start()];
    let attach =
        format!("dragonfly=127.0.0.1:{},infinitydb=127.0.0.1:{}", servers[0].port, servers[1].port);
    let output = fixture.run(&[
        "--engines",
        "redis,dragonfly,infinitydb",
        "--attach",
        &attach,
        "--workload",
        "json-set",
        "--pipeline",
        "1",
    ]);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let events = std::fs::read_to_string(fixture.0.join("events")).unwrap();
    let ports: Vec<u16> = events.lines().map(|line| line.parse().unwrap()).collect();
    assert_eq!(ports, [0, 1, 1, 0, 0, 1].map(|index| servers[index].port));
    let manifest = std::fs::read_to_string(fixture.run_dir().join("schedule.tsv")).unwrap();
    assert_eq!(manifest.lines().filter(|line| line.contains("skipped: no JSON")).count(), 3);
}

#[test]
fn entirely_unsupported_selection_cannot_succeed_and_attach_ignores_launch_ports() {
    let server = Server::start();
    let attach = format!("redis=127.0.0.1:{}", server.port);
    let fixture = Fixture::new("unsupported-selection");
    let output = fixture.run(&[
        "--attach",
        &attach,
        "--workload",
        "mixed",
        "--pipeline",
        "1",
        "--generator",
        "redis-benchmark",
    ]);
    assert!(!output.status.success());
    assert!(!fixture.run_dir().join("report.md").exists());
    let fixture = Fixture::new("attach-port-unused");
    let output = fixture.run(&[
        "--attach",
        &attach,
        "--workload",
        "set",
        "--pipeline",
        "1",
        "--port-base",
        "65535",
    ]);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
}
