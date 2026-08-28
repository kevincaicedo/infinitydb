//! ADR-0094 D2 at the binary level: the key-hash secret's lifecycle on
//! a data directory — the first boot creates `key-hash.toml` before the
//! catalog, the second boot reads the same secret, and a directory that
//! holds data without the file (one that predates the ADR) is refused
//! with the typed message. `--device-probe off` keeps every boot on the
//! dev tier so no probe runs; the data directories live under the
//! workspace `target/` like the device-probe test's.
#![cfg(target_os = "linux")]

use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").expect("bind").local_addr().expect("addr").port()
}

fn data_root(tag: &str) -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/e2e-key-hash")
        .join(format!("{tag}-{}-{}", std::process::id(), free_port()));
    let _ = std::fs::remove_dir_all(&root);
    root
}

#[derive(Debug)]
struct Server {
    child: Child,
    #[allow(dead_code)]
    port: u16,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Spawn `infinityd` on `dir` and wait for the port or the exit; a
/// refusal returns the exit status and stderr.
fn spawn(dir: &Path) -> Result<(Server, String), (i32, String)> {
    let port = free_port();
    std::fs::create_dir_all(dir).expect("data dir");
    let stderr = dir.join(format!("stderr-{port}"));
    let mut child = Command::new(env!("CARGO_BIN_EXE_infinityd"))
        .args(["--port", &port.to_string(), "--cells", "1", "--data-dir"])
        .arg(dir)
        .args(["--device-probe", "off"])
        .stdout(Stdio::null())
        .stderr(Stdio::from(std::fs::File::create(&stderr).expect("stderr file")))
        .spawn()
        .expect("spawn infinityd");
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            let text = std::fs::read_to_string(&stderr).unwrap_or_default();
            return Ok((Server { child, port }, text));
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

/// First boot creates the secret; the second reads it back unchanged
/// and says so; the file carries the schema and function it claims.
#[test]
fn the_first_boot_creates_the_secret_and_later_boots_read_it() {
    let dir = data_root("lifecycle");
    let file = dir.join("key-hash.toml");
    let first = {
        let (_server, stderr) = spawn(&dir).expect("first boot");
        assert!(
            stderr.contains("key-hash secret: key-hash.toml (created at this first boot"),
            "{stderr}"
        );
        std::fs::read_to_string(&file).expect("the secret file exists after the first boot")
    };
    assert!(first.contains("schema = 1\n"), "{first}");
    assert!(first.contains("function = \"siphash13\"\n"), "{first}");
    assert!(first.contains("k0 = 0x") && first.contains("k1 = 0x"), "{first}");
    assert!(!dir.join("key-hash.toml.tmp").exists(), "no temp residue");
    {
        let (_server, stderr) = spawn(&dir).expect("second boot");
        assert!(stderr.contains("key-hash secret: key-hash.toml (read"), "{stderr}");
    }
    assert_eq!(std::fs::read_to_string(&file).expect("still there"), first, "unchanged");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A directory that holds data without the secret predates ADR-0094:
/// refused with the typed message, exit 1, and no file written over it.
#[test]
fn a_directory_that_predates_the_secret_is_refused() {
    let dir = data_root("predates");
    // Boot once to lay down a real catalog and shard, then remove the
    // secret — the shape of a directory written by a pre-ADR binary.
    {
        let (_server, _) = spawn(&dir).expect("first boot");
    }
    assert!(dir.join("META").exists() || dir.join("shard-0").exists(), "data was written");
    std::fs::remove_file(dir.join("key-hash.toml")).expect("remove the secret");
    let (code, stderr) = spawn(&dir).expect_err("a pre-ADR directory is refused");
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("predates keyed key hashing (ADR-0094)"), "{stderr}");
    assert!(!dir.join("key-hash.toml").exists(), "no secret written over existing data");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A malformed secret is a refusal with the line named, never a fresh
/// secret over existing refs.
#[test]
fn a_malformed_secret_is_refused() {
    let dir = data_root("malformed");
    std::fs::create_dir_all(&dir).expect("dir");
    std::fs::write(dir.join("key-hash.toml"), "schema = 1\nfunction = \"siphash13\"\nk0 = 1\n")
        .expect("write");
    let (code, stderr) = spawn(&dir).expect_err("refused");
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("key-hash.toml: `k1` missing"), "{stderr}");
    let _ = std::fs::remove_dir_all(&dir);
}
