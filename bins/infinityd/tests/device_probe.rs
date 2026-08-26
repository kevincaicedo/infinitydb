//! M4.5-S42 (ADR-0091 D4): the device-model lifecycle at the binary
//! level — `infinityd --device-probe auto` on a fresh data directory
//! probes, writes a schema-3 `io-properties.toml`, boots, and says so in
//! `INFO persistence`; the second boot reads the file; `off` on an
//! absent file boots the dev tier; a file carrying a foreign identity
//! refuses under `off` and re-probes (leaving `.stale`) under `auto`.
//!
//! Linux only (the direct class); the data directories live under the
//! workspace `target/` (a real filesystem — `/tmp` may be a quota-bound
//! tmpfs on the dev box, and a tmpfs probe would only ever measure the
//! refused-direct-class branch). Each `auto` boot pays the probe
//! (≈ 9 rows × 1 s + a 256 MiB pre-write), so the test runs three of
//! them and no more.
#![cfg(target_os = "linux")]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").expect("bind").local_addr().expect("addr").port()
}

fn data_root() -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/e2e-device-probe")
        .join(format!("{}-{}", std::process::id(), free_port()));
    std::fs::create_dir_all(&root).expect("data root");
    root
}

struct Server {
    child: Child,
    port: u16,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Spawn `infinityd` and wait for the port (the probe makes the first
/// boot slow — a generous deadline) or for the process to exit; returns
/// the exit status and stderr on a refusal.
fn spawn(dir: &Path, extra: &[&str]) -> Result<Server, (i32, String)> {
    let port = free_port();
    let stderr = dir.join(format!("stderr-{port}"));
    let mut child = Command::new(env!("CARGO_BIN_EXE_infinityd"))
        .args(["--port", &port.to_string(), "--cells", "1", "--data-dir"])
        .arg(dir)
        .args(["--probe-seconds", "1"])
        .args(extra)
        .stdout(Stdio::null())
        .stderr(Stdio::from(std::fs::File::create(&stderr).expect("stderr file")))
        .spawn()
        .expect("spawn infinityd");
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return Ok(Server { child, port });
        }
        if let Some(status) = child.try_wait().expect("try_wait") {
            let text = std::fs::read_to_string(&stderr).unwrap_or_default();
            return Err((status.code().unwrap_or(-1), text));
        }
        assert!(Instant::now() < deadline, "infinityd never came up");
        #[allow(clippy::disallowed_methods)] // test harness, not cell code
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn info_persistence(port: u16) -> String {
    let mut conn = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    conn.set_read_timeout(Some(Duration::from_secs(5))).expect("timeout");
    conn.write_all(b"*2\r\n$4\r\nINFO\r\n$11\r\npersistence\r\n").expect("write");
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let started = Instant::now();
    loop {
        match conn.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                // A bulk string: `$len\r\n…\r\n` — done once the payload is in.
                if let Some(end) = buf.iter().position(|&b| b == b'\n')
                    && let Ok(len) = String::from_utf8_lossy(&buf[1..end - 1]).parse::<usize>()
                    && buf.len() >= end + 1 + len + 2
                {
                    break;
                }
            }
            Err(_) => break,
        }
        assert!(started.elapsed() < Duration::from_secs(10), "INFO never completed");
    }
    String::from_utf8_lossy(&buf).into_owned()
}

/// `INFO persistence` once `key` reads `expected` (the durable stats
/// reach `NodeInfo` on the MAINTAIN slice — an INFO sent in the accept's
/// own iteration can precede the first publish), or the last reading
/// after 5 s so the assertion names the real value.
fn info_when(port: u16, key: &str, expected: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let info = info_persistence(port);
        if field(&info, key) == expected || Instant::now() >= deadline {
            return info;
        }
        #[allow(clippy::disallowed_methods)] // test harness, not cell code
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn field<'a>(info: &'a str, key: &str) -> &'a str {
    info.lines()
        .find_map(|line| line.strip_prefix(key).and_then(|rest| rest.strip_prefix(':')))
        .unwrap_or_else(|| panic!("{key} missing from INFO persistence:\n{info}"))
        .trim()
}

/// The lifecycle, end to end, on one data directory.
#[test]
fn a_fresh_data_directory_is_probed_once_and_the_model_is_identity_bound() {
    let dir = data_root();

    // 1. `off` on an absent file: the dev tier, byte-for-byte the
    //    pre-S42 boot — no file is written.
    {
        let server = spawn(&dir, &["--device-probe", "off"]).expect("off boots");
        let info = info_when(server.port, "io_properties_source", "absent");
        assert_eq!(field(&info, "io_properties_source"), "absent");
        assert_eq!(field(&info, "io_properties_schema"), "1");
        assert_eq!(field(&info, "io_properties_identity"), "unverifiable");
        assert_eq!(field(&info, "barrier_class"), "flush");
        assert_eq!(field(&info, "io_budget_model"), "absent");
        // The FLUSH class carries the ADR-0092 group hold by default
        // since campaign K (2026-08-26): 250 µs unless `--flush-group-
        // window-us 0` — the one line of the `off` tier that is not the
        // pre-S42 boot byte-for-byte.
        assert_eq!(field(&info, "flush_group_window_us"), "250");
        assert!(!dir.join("io-properties.toml").exists(), "off never writes the file");
    }

    // 2. `auto` (the default) on the same directory: the probe runs
    //    once, before the cell, and the boot reports it.
    {
        let server = spawn(&dir, &[]).expect("auto boots");
        let info = info_when(server.port, "io_properties_source", "probed-at-boot");
        assert_eq!(field(&info, "io_properties_source"), "probed-at-boot");
        assert_eq!(field(&info, "io_properties_schema"), "3");
        assert_eq!(field(&info, "io_properties_identity"), "verified");
        let text = std::fs::read_to_string(dir.join("io-properties.toml")).expect("written");
        assert!(text.contains("probe_schema = 3\n"), "{text}");
        assert!(text.contains("fs_type = \""), "{text}");
        assert!(text.contains("fua_p50_us_512 = "), "{text}");
        // The configured class is the probe's own verdict. INFO's
        // `barrier_class` names the *active segment's* class, and a fresh
        // cell's segment 0 stays FLUSH-class until a pre-zeroed segment
        // exists (ADR-0086 D4; zero-fill waits for an `always` namespace,
        // ADR-0088 D5) — so the configured class's witness is the
        // class-derived K (ADR-0087: FUA → 3, FLUSH → 1) and the model.
        let verdict = text
            .lines()
            .find_map(|l| l.strip_prefix("barrier_class = "))
            .expect("class line")
            .trim_matches('"')
            .to_owned();
        if text.contains("fua_unsupported") {
            // The refused-direct-class branch (tmpfs and friends): flush,
            // the reason on the record, no device to budget.
            assert_eq!(verdict, "flush");
            assert_eq!(field(&info, "frames_in_flight"), "1");
        } else {
            assert_eq!(field(&info, "io_budget_model"), "probed");
            let expected_k = if verdict == "fua" { "3" } else { "1" };
            assert_eq!(field(&info, "frames_in_flight"), expected_k, "class {verdict}");
        }
    }

    // 3. The second boot reads the file — no probe.
    {
        let started = Instant::now();
        let server = spawn(&dir, &[]).expect("second boot");
        let info = info_when(server.port, "io_properties_source", "file");
        assert_eq!(field(&info, "io_properties_source"), "file");
        assert_eq!(field(&info, "io_properties_identity"), "verified");
        assert!(started.elapsed() < Duration::from_secs(8), "the second boot must not probe");
    }

    // 4. A model that describes another device (a foreign uuid): `off`
    //    refuses with the reason; `auto` renames it `.stale` and probes.
    let path = dir.join("io-properties.toml");
    let original = std::fs::read_to_string(&path).expect("file");
    let foreign = original
        .lines()
        .map(|line| {
            if line.starts_with("fs_uuid = ") {
                "fs_uuid = \"00000000-dead-beef-0000-000000000000\"".to_owned()
            } else if line.starts_with("device_path = ") {
                "device_path = \"/dev/definitely-not-this-device\"".to_owned()
            } else {
                line.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    std::fs::write(&path, &foreign).expect("write foreign");
    let (code, stderr) = spawn(&dir, &["--device-probe", "off"]).err().expect("off refuses");
    assert_eq!(code, 1, "fail-stop exit code; stderr: {stderr}");
    assert!(stderr.contains("describes another device"), "{stderr}");
    assert!(path.exists(), "a refusal leaves the file for the operator");
    {
        let server = spawn(&dir, &[]).expect("auto re-probes");
        let info = info_when(server.port, "io_properties_source", "re-probed");
        assert_eq!(field(&info, "io_properties_source"), "re-probed");
        assert_eq!(field(&info, "io_properties_identity"), "verified");
        assert!(dir.join("io-properties.toml.stale").exists(), "the stale model is kept");
        let fresh = std::fs::read_to_string(&path).expect("re-probed file");
        assert!(!fresh.contains("dead-beef"), "the foreign identity is gone: {fresh}");
    }

    let _ = std::fs::remove_dir_all(dir.parent().expect("root"));
}
