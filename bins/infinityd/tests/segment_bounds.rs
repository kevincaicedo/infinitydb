#![allow(clippy::disallowed_methods, reason = "test target: process deadline, not cell code")]
#![cfg(target_os = "linux")]
//! Real startup refusal for a sparse segment outside the log address range.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Boot {
    child: Child,
    root: PathBuf,
}

impl Drop for Boot {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        std::fs::remove_dir_all(&self.root).expect("remove scratch directory");
    }
}

/// A planted 4 GiB file must produce a named refusal, with its bytes preserved.
#[test]
fn oversized_segment_boot_refuses_without_panic_or_log_changes() {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("segment-bounds-{}", std::process::id()));
    std::fs::create_dir(&root).expect("unique scratch directory");
    inf_server::create_key_hash(&root, inf_foundation::KeyHasher::from_seed(16))
        .expect("directory hash binding");
    let log_dir = root.join("shard-0/log");
    std::fs::create_dir_all(&log_dir).expect("log directory");
    let segment = log_dir.join("seg-000000.ilog");
    std::fs::File::create(&segment).unwrap().set_len(1u64 << 32).unwrap();
    let stderr = root.join("stderr");
    let child = Command::new(env!("CARGO_BIN_EXE_infinityd"))
        .args(["--port", "0", "--cells", "1", "--data-dir"])
        .arg(&root)
        .args(["--device-probe", "off"])
        .stdout(Stdio::null())
        .stderr(Stdio::from(std::fs::File::create(&stderr).unwrap()))
        .spawn()
        .expect("spawn infinityd");
    let mut boot = Boot { child, root };
    let deadline = Instant::now() + Duration::from_secs(45);
    let status = loop {
        if let Some(status) = boot.child.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "boot did not refuse oversized segment within 45 s");
        std::thread::sleep(Duration::from_millis(20));
    };
    let text = std::fs::read_to_string(stderr).unwrap();
    assert!(!status.success(), "oversized segment boot succeeded: {text}");
    assert!(!text.contains("panicked"), "startup panicked: {text}");
    assert!(text.contains("exceeds u32 segment address limit"), "missing named refusal: {text}");
    assert!(text.contains("4294967296"), "missing observed length: {text}");
    assert_eq!(std::fs::metadata(segment).unwrap().len(), 1u64 << 32);
    assert_eq!(std::fs::read_dir(log_dir).unwrap().count(), 1);
    println!("segment bounds binary: named refusal; no panic; log unchanged");
}
