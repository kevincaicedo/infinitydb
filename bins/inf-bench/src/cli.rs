//! Minimal `--flag value` parsing. No CLI dependency: the flag surface is
//! small, fixed, and validated against a per-subcommand allowlist so typos
//! fail loudly instead of being silently ignored.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub struct Flags {
    values: BTreeMap<String, String>,
}

// Some accessors are reserved for the M0-S18 `load`/`gate-run` subcommands
// (in progress); only env-check links this module today.
#[allow(dead_code, reason = "accessors reserved for the load/gate-run subcommands")]
impl Flags {
    /// Parse `args` into flag/value pairs. Flags named in `bool_flags` take
    /// no value; every flag must appear in `known` (which includes the bool
    /// flags). Positional arguments are rejected — subcommands strip theirs
    /// before calling this.
    pub fn parse(args: &[String], bool_flags: &[&str], known: &[&str]) -> Result<Flags, String> {
        let mut values = BTreeMap::new();
        let mut it = args.iter();
        while let Some(arg) = it.next() {
            let Some(name) = arg.strip_prefix("--") else {
                return Err(format!("unexpected argument `{arg}` (flags are `--name value`)"));
            };
            if !known.contains(&name) {
                return Err(format!("unknown flag `--{name}` (known: {})", known.join(", ")));
            }
            let value = if bool_flags.contains(&name) {
                "true".to_string()
            } else {
                let Some(v) = it.next() else {
                    return Err(format!("flag `--{name}` needs a value"));
                };
                v.clone()
            };
            if values.insert(name.to_string(), value).is_some() {
                return Err(format!("flag `--{name}` given twice"));
            }
        }
        Ok(Flags { values })
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(String::as_str)
    }

    pub fn str_or(&self, name: &str, default: &str) -> String {
        self.get(name).unwrap_or(default).to_string()
    }

    pub fn bool(&self, name: &str) -> bool {
        self.get(name).is_some()
    }

    pub fn require(&self, name: &str) -> Result<&str, String> {
        self.get(name).ok_or_else(|| format!("missing required flag `--{name}`"))
    }

    pub fn u64_or(&self, name: &str, default: u64) -> Result<u64, String> {
        parse_num(self.get(name), name, default)
    }

    pub fn usize_or(&self, name: &str, default: usize) -> Result<usize, String> {
        parse_num(self.get(name), name, default)
    }

    pub fn u16_or(&self, name: &str, default: u16) -> Result<u16, String> {
        parse_num(self.get(name), name, default)
    }

    pub fn f64_or(&self, name: &str, default: f64) -> Result<f64, String> {
        parse_num(self.get(name), name, default)
    }
}

#[allow(dead_code)] // reserved with the numeric flag accessors above (M0-S18)
fn parse_num<T: std::str::FromStr>(raw: Option<&str>, name: &str, default: T) -> Result<T, String> {
    match raw {
        None => Ok(default),
        Some(s) => s.parse().map_err(|_| format!("flag `--{name}`: `{s}` is not a valid number")),
    }
}

/// The `--infinityd-bin` default: the release `infinityd` in the target root
/// that built this binary. A working directory's `target/release/infinityd` is
/// the default root's only: under `CARGO_TARGET_DIR` it is a stale binary or
/// none, and a gate would measure it (ADR-0198 A2). `infinityd` on `PATH` when
/// the running binary's path is unknown.
pub fn default_infinityd_bin() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|exe| release_beside(&exe, "infinityd"))
        .map_or_else(|| "infinityd".to_string(), |path| path.display().to_string())
}

/// Cargo puts a binary at `<root>/[<target triple>/]<profile>/<name>` and a
/// test binary one level down, in `deps/`, so `<name>`'s release build sits in
/// the `release` sibling of the directory holding `exe` (or holding its `deps/`).
fn release_beside(exe: &Path, name: &str) -> Option<PathBuf> {
    let mut profile = exe.parent()?;
    if profile.file_name().is_some_and(|leaf| leaf == "deps") {
        profile = profile.parent()?;
    }
    Some(profile.parent()?.join("release").join(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_server_default_is_the_release_build_in_this_binarys_own_root() {
        let at = |exe: &str| release_beside(Path::new(exe), "infinityd");
        assert_eq!(at("/r/release/inf-bench"), Some(PathBuf::from("/r/release/infinityd")));
        assert_eq!(at("/r/debug/inf-bench"), Some(PathBuf::from("/r/release/infinityd")));
        assert_eq!(at("/r/debug/deps/inf_bench-0a1b"), Some(PathBuf::from("/r/release/infinityd")));
        assert_eq!(
            at("/shared/checkout-0123/release/inf-bench"),
            Some(PathBuf::from("/shared/checkout-0123/release/infinityd"))
        );
        assert_eq!(at("/"), None);
        assert_eq!(at("inf-bench"), None);
        // The running test binary's own root, never the working directory's.
        let exe = std::env::current_exe().unwrap();
        let root = exe.parent().and_then(Path::parent).and_then(Path::parent).unwrap();
        assert_eq!(default_infinityd_bin(), root.join("release/infinityd").display().to_string());
    }

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_values_and_bools() {
        let f = Flags::parse(
            &args(&["--port", "6379", "--allow-dirty"]),
            &["allow-dirty"],
            &["port", "allow-dirty"],
        )
        .unwrap();
        assert_eq!(f.u16_or("port", 0).unwrap(), 6379);
        assert!(f.bool("allow-dirty"));
        assert!(!f.bool("other"));
    }

    #[test]
    fn rejects_unknown_flag() {
        let err = Flags::parse(&args(&["--bogus", "1"]), &[], &["port"]).unwrap_err();
        assert!(err.contains("--bogus"), "{err}");
    }

    #[test]
    fn rejects_missing_value_and_positionals() {
        assert!(Flags::parse(&args(&["--port"]), &[], &["port"]).is_err());
        assert!(Flags::parse(&args(&["stray"]), &[], &["port"]).is_err());
    }

    #[test]
    fn rejects_duplicate_flag() {
        let err = Flags::parse(&args(&["--port", "1", "--port", "2"]), &[], &["port"]).unwrap_err();
        assert!(err.contains("twice"), "{err}");
    }

    #[test]
    fn require_and_defaults() {
        let f = Flags::parse(&args(&[]), &[], &["port"]).unwrap();
        assert!(f.require("port").is_err());
        assert_eq!(f.u64_or("port", 7).unwrap(), 7);
        assert_eq!(f.str_or("port", "x"), "x");
    }
}
