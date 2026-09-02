//! Boot-flag gauntlet at the binary level (ADR-0107 D2, Theme 4 of the
//! full-codebase review of 2026-08-30): an operator value that reaches a
//! cell-crate constructor is validated where it can be — the flag parser —
//! and answers a usage error (exit 2, the flag named). Before this test
//! `--buffers 0` and `--buf-size 0` booted into a release `assert!` inside
//! `inf-alloc` and died with a panic backtrace (exit 101), and `--cells`
//! above `SLOT_COUNT` never reached `inf-store`'s router assert at all: the
//! fabric mesh (cells² rings × 4096 slots) is built first, and the
//! red-first run of this test at `--cells 20000` drove `infinityd` to
//! ~30 GB of resident memory before the kernel OOM-killed it (2026-09-02,
//! twice). The inventory's `buffer_pool.rs` and `router.rs` C rows cite
//! `parse_args` as the enforcing check; this is the test that makes the
//! citation true.
//!
//! Harness rule (memory discipline): every child runs under a 4 GiB
//! address-space cap (`ulimit -v`), so a boot that allocates before it
//! validates dies typed (SIGABRT on allocation failure) instead of taking
//! the box down. The cases must exit at the parser, before any cell or
//! mesh allocation, so the cap is invisible to a correct binary.
#![cfg(target_os = "linux")]
use std::path::{Path, PathBuf};
use std::process::Command;

/// 4 GiB in KiB — `ulimit -v` units.
const CHILD_AS_CAP_KIB: u64 = 4 * 1024 * 1024;

fn scratch(tag: &str) -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/e2e-boot-flags")
        .join(format!("{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("scratch dir");
    root
}

/// Runs `infinityd` with `flags` under the address-space cap and returns
/// (exit code, stderr). A signal death reports `None`.
fn boot(tag: &str, flags: &[&str]) -> (Option<i32>, String) {
    let dir = scratch(tag);
    let out = Command::new("sh")
        .arg("-c")
        .arg(format!("ulimit -v {CHILD_AS_CAP_KIB} && exec \"$0\" \"$@\""))
        .arg(env!("CARGO_BIN_EXE_infinityd"))
        .args(["--port", "0", "--device-probe", "off", "--data-dir"])
        .arg(&dir)
        .args(flags)
        .output()
        .expect("spawn infinityd");
    let _ = std::fs::remove_dir_all(&dir);
    (out.status.code(), String::from_utf8_lossy(&out.stderr).into_owned())
}

#[test]
fn out_of_range_boot_flags_are_usage_errors_not_panics() {
    let cases: &[(&str, &[&str], &str)] = &[
        ("buffers-zero", &["--buffers", "0"], "--buffers must be >= 1"),
        ("buffers-over", &["--buffers", "4294967296"], "--buffers must be <= 4294967295"),
        ("buf-size-zero", &["--buf-size", "0"], "--buf-size must be >= 1"),
        ("cells-over", &["--cells", "16385"], "--cells must be <= 16384"),
    ];
    let mut failures = Vec::new();
    for (tag, flags, expect) in cases {
        let (code, stderr) = boot(tag, flags);
        let ok = code == Some(2) && stderr.contains(expect) && !stderr.contains("panicked");
        if !ok {
            failures.push(format!(
                "{tag}: {flags:?} → exit {code:?}, expected exit 2 naming {expect:?}; stderr:\n{}",
                stderr.lines().take(6).collect::<Vec<_>>().join("\n")
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n---\n"));
}
