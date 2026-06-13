//! Build provenance for `infinityd --version` (M1-S14): release version,
//! git SHA (+`-dirty` marker), and build target are stamped into the binary
//! at compile time. The release pipeline injects `INF_RELEASE_VERSION` from
//! the tag; dev builds fall back to `git describe`, then the crate version.
//! No wall-clock build timestamp: builds stay reproducible (provenance is
//! the commit, not the minute).

use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn main() {
    let version = std::env::var("INF_RELEASE_VERSION")
        .ok()
        .filter(|v| !v.is_empty())
        .or_else(|| git(&["describe", "--tags", "--always", "--match", "v*"]))
        .unwrap_or_else(|| std::env::var("CARGO_PKG_VERSION").expect("cargo sets PKG_VERSION"));
    // Docker builds exclude .git from the context; the pipeline forwards the
    // SHA as a build arg instead.
    let sha = std::env::var("INF_GIT_SHA").ok().filter(|v| !v.is_empty()).unwrap_or_else(|| {
        let sha = git(&["rev-parse", "--short=12", "HEAD"]).unwrap_or_else(|| "unknown".into());
        let dirty = git(&["status", "--porcelain"]).is_some_and(|s| !s.is_empty());
        format!("{sha}{}", if dirty { "-dirty" } else { "" })
    });
    println!("cargo::rustc-env=INF_VERSION={version}");
    println!("cargo::rustc-env=INF_GIT_SHA={sha}");
    println!(
        "cargo::rustc-env=INF_BUILD_TARGET={}",
        std::env::var("TARGET").expect("cargo sets TARGET")
    );
    println!("cargo::rerun-if-env-changed=INF_RELEASE_VERSION");
    println!("cargo::rerun-if-env-changed=INF_GIT_SHA");
    if let Some(dir) = git(&["rev-parse", "--git-dir"]) {
        println!("cargo::rerun-if-changed={dir}/HEAD");
    }
}
