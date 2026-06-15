//! Minimal `--flag value` parsing. No CLI dependency: the flag surface is
//! small and validated against a per-subcommand allowlist so typos fail loudly
//! instead of being silently ignored. Same shape as `inf-bench`'s parser.

use std::collections::BTreeMap;

#[derive(Debug)]
pub struct Flags {
    values: BTreeMap<String, String>,
}

impl Flags {
    /// Parse `args` into flag/value pairs. Flags in `bool_flags` take no value;
    /// every flag must appear in `known`. Positional args are rejected.
    pub fn parse(args: &[String], bool_flags: &[&str], known: &[&str]) -> Result<Flags, String> {
        let mut values = BTreeMap::new();
        let mut it = args.iter();
        while let Some(arg) = it.next() {
            let Some(name) = arg.strip_prefix("--") else {
                return Err(format!("unexpected argument `{arg}` (flags are `--name value`)"));
            };
            if !known.contains(&name) {
                return Err(format!("unknown flag `--{name}`\n  known: {}", known.join(", ")));
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

    pub fn u64_or(&self, name: &str, default: u64) -> Result<u64, String> {
        parse_num(self.get(name), name, default)
    }

    pub fn u16_or(&self, name: &str, default: u16) -> Result<u16, String> {
        parse_num(self.get(name), name, default)
    }

    pub fn usize_or(&self, name: &str, default: usize) -> Result<usize, String> {
        parse_num(self.get(name), name, default)
    }

    pub fn f64_or(&self, name: &str, default: f64) -> Result<f64, String> {
        parse_num(self.get(name), name, default)
    }

    /// Optional numeric flag: `None` when absent, error when present-but-bad.
    pub fn opt_u64(&self, name: &str) -> Result<Option<u64>, String> {
        match self.get(name) {
            None => Ok(None),
            Some(s) => {
                s.parse().map(Some).map_err(|_| format!("flag `--{name}`: `{s}` is not a number"))
            }
        }
    }

    pub fn opt_usize(&self, name: &str) -> Result<Option<usize>, String> {
        match self.get(name) {
            None => Ok(None),
            Some(s) => {
                s.parse().map(Some).map_err(|_| format!("flag `--{name}`: `{s}` is not a number"))
            }
        }
    }

    /// Parse a comma list like `1,16` into `u32`s, or `default` if absent.
    pub fn u32_list_or(&self, name: &str, default: &[u32]) -> Result<Vec<u32>, String> {
        match self.get(name) {
            None => Ok(default.to_vec()),
            Some(s) => s
                .split(',')
                .map(|p| {
                    p.trim()
                        .parse::<u32>()
                        .map_err(|_| format!("flag `--{name}`: `{p}` is not a number"))
                })
                .collect(),
        }
    }
}

fn parse_num<T: std::str::FromStr>(raw: Option<&str>, name: &str, default: T) -> Result<T, String> {
    match raw {
        None => Ok(default),
        Some(s) => s.parse().map_err(|_| format!("flag `--{name}`: `{s}` is not a valid number")),
    }
}
