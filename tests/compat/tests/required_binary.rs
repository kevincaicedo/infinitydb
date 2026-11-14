//! Required binary execution must fail when its candidate is absent.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::process::Command;

/// The values the harness starts nothing from: unset, empty, not Unicode.
fn unusable_binaries() -> [Option<&'static OsStr>; 3] {
    [None, Some(OsStr::new("")), Some(OsStr::from_bytes(b"\xff"))]
}

#[test]
fn required_binary_cannot_skip() {
    for flag in ["1", "0", "bogus"] {
        for binary in unusable_binaries() {
            let mut child = child_command();
            child.env("INF_COMPAT_REQUIRE_BINARY", flag);
            if let Some(binary) = binary {
                child.env("INFINITYD_BIN", binary);
            }
            let output = child.output().expect("run child");
            assert!(!output.status.success(), "H3 follow-up: required binary silently skipped");
            let message = if flag == "1" {
                "INF_COMPAT_REQUIRE_BINARY=1 requires INFINITYD_BIN"
            } else {
                "INF_COMPAT_REQUIRE_BINARY must be unset or 1"
            };
            assert!(String::from_utf8_lossy(&output.stderr).contains(message));
        }
    }
}

/// An optional lane skips on every value it cannot start: an empty or a
/// non-Unicode one as an unset one, never a spawn of a path that names no
/// binary.
#[test]
fn optional_binary_can_skip() {
    for binary in unusable_binaries() {
        let mut child = child_command();
        if let Some(binary) = binary {
            child.env("INFINITYD_BIN", binary);
        }
        let output = child.output().expect("run child");
        assert!(
            output.status.success(),
            "INFINITYD_BIN {binary:?} did not skip: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

fn child_command() -> Command {
    let mut child = Command::new(std::env::current_exe().expect("test executable"));
    child
        .args(["--exact", "binary_requirement_child", "--nocapture"])
        .env_remove("INFINITYD_BIN")
        .env_remove("INF_COMPAT_REQUIRE_BINARY")
        .env("INF_COMPAT_REQUIRE_CHILD", "1");
    child
}

#[test]
fn binary_requirement_child() {
    if std::env::var_os("INF_COMPAT_REQUIRE_CHILD").is_some() {
        let _ = compat::harness::infinityd(2, Path::new(env!("CARGO_TARGET_TMPDIR")));
    }
}
