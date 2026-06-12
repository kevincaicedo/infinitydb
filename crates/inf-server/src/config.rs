//! Typed CONFIG store (M1-S03, milestone §3.2 freeze): a fixed table of
//! known keys, each with a **hot-reload class** — later milestones add keys,
//! never new mechanisms. Values live as their canonical Redis string form
//! (CONFIG GET echoes what Redis would); typed validation runs at SET.
//!
//! Cell-local behind `Rc<NodeInfo>` (no locks — L1). `maxmemory` /
//! `maxmemory-policy` are stored and reported now; the eviction engine
//! (M1-E3) consumes them when it lands — until then they are configuration
//! state, not behavior (kept honest in the compat matrix).

use crate::glob::glob_match;

/// When a key's new value takes effect (frozen vocabulary).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum ReloadClass {
    /// Only readable at runtime; set at boot.
    BootOnly,
    /// Applies node-wide on the next control sweep.
    Hot,
    /// Applies per cell within one MAINTAIN round.
    HotPerCell,
}

/// Validation/normalization rule per key.
#[derive(Copy, Clone, Debug)]
enum Kind {
    /// Byte size with Redis memory units (`100mb` → `104857600`).
    Memory,
    /// Plain integer.
    Int,
    /// One of a fixed token set (case-insensitive, stored lowercase).
    Enum(&'static [&'static str]),
    /// Free-form string.
    Str,
    /// `client-output-buffer-limit` class triples (M1-S11): SET merges the
    /// given `<class> <hard> <soft> <secs>` groups onto the current value;
    /// GET reports all three classes in canonical order, like Redis.
    OutputBufferLimit,
}

#[derive(Debug)]
struct Entry {
    key: &'static str,
    class: ReloadClass,
    kind: Kind,
    value: String,
}

/// `CONFIG SET` failure, mapped to Redis error strings by the caller.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ConfigSetError {
    Unknown(String),
    Immutable(String),
    Invalid { key: String, value: String },
}

/// All eight Redis eviction policies (M1-E3 consumes the selection).
pub const MAXMEMORY_POLICIES: &[&str] = &[
    "noeviction",
    "allkeys-lru",
    "volatile-lru",
    "allkeys-random",
    "volatile-random",
    "volatile-ttl",
    "allkeys-lfu",
    "volatile-lfu",
];

/// The M1 key subset. Defaults mirror Redis 8 so `CONFIG GET` byte-diffs.
#[derive(Debug)]
pub struct ConfigStore {
    entries: Vec<Entry>,
    /// Bumped on every successful SET — the plane's MAINTAIN sweep compares
    /// it to push `hot-per-cell` keys (eviction pressure) without re-parsing
    /// the table each iteration (M1-E3).
    version: u64,
}

impl Default for ConfigStore {
    fn default() -> ConfigStore {
        let e = |key, class, kind, value: &str| Entry { key, class, kind, value: value.into() };
        ConfigStore {
            version: 0,
            entries: vec![
                e("appendonly", ReloadClass::BootOnly, Kind::Enum(&["no", "yes"]), "no"),
                e(
                    "client-output-buffer-limit",
                    ReloadClass::HotPerCell,
                    Kind::OutputBufferLimit,
                    "normal 0 0 0 slave 268435456 67108864 60 pubsub 33554432 8388608 60",
                ),
                e("databases", ReloadClass::BootOnly, Kind::Int, "16"),
                e("maxclients", ReloadClass::BootOnly, Kind::Int, "10000"),
                e("maxmemory", ReloadClass::HotPerCell, Kind::Memory, "0"),
                e(
                    "maxmemory-policy",
                    ReloadClass::HotPerCell,
                    Kind::Enum(MAXMEMORY_POLICIES),
                    "noeviction",
                ),
                e("maxmemory-samples", ReloadClass::HotPerCell, Kind::Int, "5"),
                e("proto-max-bulk-len", ReloadClass::Hot, Kind::Memory, "536870912"),
                e("save", ReloadClass::Hot, Kind::Str, "3600 1 300 100 60 10000"),
                e("tcp-keepalive", ReloadClass::Hot, Kind::Int, "300"),
                e("timeout", ReloadClass::Hot, Kind::Int, "0"),
            ],
        }
    }
}

impl ConfigStore {
    /// Keys matching any of `patterns` (nocase glob, Redis CONFIG GET),
    /// deduplicated, in table (alphabetical) order.
    pub fn get_matching(&self, patterns: &[&[u8]]) -> Vec<(&'static str, &str)> {
        self.entries
            .iter()
            .filter(|e| patterns.iter().any(|p| glob_match(p, e.key.as_bytes(), true)))
            .map(|e| (e.key, e.value.as_str()))
            .collect()
    }

    /// Direct read (engine consumers — eviction reads `maxmemory*`).
    pub fn get(&self, key: &str) -> Option<&str> {
        self.entries.iter().find(|e| e.key == key).map(|e| e.value.as_str())
    }

    /// Validates and stores. The caller maps errors to Redis reply strings.
    pub fn set(&mut self, key: &[u8], value: &[u8]) -> Result<ReloadClass, ConfigSetError> {
        let key_str = String::from_utf8_lossy(key).to_lowercase();
        let Some(entry) = self.entries.iter_mut().find(|e| e.key == key_str) else {
            return Err(ConfigSetError::Unknown(key_str));
        };
        if entry.class == ReloadClass::BootOnly {
            return Err(ConfigSetError::Immutable(key_str));
        }
        let text = String::from_utf8_lossy(value).to_string();
        let normalized = match entry.kind {
            Kind::Memory => parse_memory(&text).map(|b| b.to_string()).ok_or_else(|| {
                ConfigSetError::Invalid { key: key_str.clone(), value: text.clone() }
            })?,
            Kind::Int => text.parse::<i64>().map(|v| v.to_string()).map_err(|_| {
                ConfigSetError::Invalid { key: key_str.clone(), value: text.clone() }
            })?,
            Kind::Enum(tokens) => {
                let lower = text.to_lowercase();
                if !tokens.contains(&lower.as_str()) {
                    return Err(ConfigSetError::Invalid { key: key_str, value: text });
                }
                lower
            }
            Kind::Str => text,
            Kind::OutputBufferLimit => merge_output_buffer_limit(&entry.value, &text)
                .ok_or(ConfigSetError::Invalid { key: key_str, value: text })?,
        };
        entry.value = normalized;
        self.version += 1;
        Ok(entry.class)
    }

    /// Monotone mutation counter (see the field note).
    #[inline]
    pub fn version(&self) -> u64 {
        self.version
    }
}

/// `client-output-buffer-limit` value as `(class_index, [hard, soft,
/// secs])` groups. Classes: 0 = normal, 1 = slave (replica normalizes), 2 =
/// pubsub.
fn parse_output_buffer_groups(text: &str) -> Option<Vec<(usize, [u64; 3])>> {
    let tokens: Vec<&str> = text.split_whitespace().collect();
    if tokens.is_empty() || !tokens.len().is_multiple_of(4) {
        return None;
    }
    let mut groups = Vec::with_capacity(tokens.len() / 4);
    for chunk in tokens.chunks(4) {
        let class = match chunk[0].to_lowercase().as_str() {
            "normal" => 0,
            "slave" | "replica" => 1,
            "pubsub" => 2,
            _ => return None,
        };
        let hard = parse_memory(chunk[1])?;
        let soft = parse_memory(chunk[2])?;
        let secs: u64 = chunk[3].parse().ok()?;
        groups.push((class, [hard, soft, secs]));
    }
    Some(groups)
}

/// Merge-and-normalize for `CONFIG SET client-output-buffer-limit` (the
/// Redis semantics: only the named classes change).
fn merge_output_buffer_limit(current: &str, update: &str) -> Option<String> {
    let mut values = [[0u64; 3]; 3];
    for (class, v) in parse_output_buffer_groups(current)? {
        values[class] = v;
    }
    for (class, v) in parse_output_buffer_groups(update)? {
        values[class] = v;
    }
    let rendered: Vec<String> = ["normal", "slave", "pubsub"]
        .iter()
        .zip(values)
        .map(|(name, [hard, soft, secs])| format!("{name} {hard} {soft} {secs}"))
        .collect();
    Some(rendered.join(" "))
}

/// The pubsub class triple `(hard, soft, soft_ms)` for the plane's
/// output-cap enforcement (M1-S11). Zeros disable a limit.
pub(crate) fn pubsub_output_limit(cfg: &ConfigStore) -> (u64, u64, u64) {
    let Some(text) = cfg.get("client-output-buffer-limit") else { return (0, 0, 0) };
    let Some(groups) = parse_output_buffer_groups(text) else { return (0, 0, 0) };
    groups
        .iter()
        .rev()
        .find(|(class, _)| *class == 2)
        .map_or((0, 0, 0), |(_, [hard, soft, secs])| (*hard, *soft, secs.saturating_mul(1000)))
}

/// Redis memory-unit grammar: bare bytes, or `k/kb/m/mb/g/gb` suffixes
/// (decimal vs binary multipliers, case-insensitive).
pub(crate) fn parse_memory(text: &str) -> Option<u64> {
    let lower = text.to_lowercase();
    let (digits, mult) = match lower {
        _ if lower.ends_with("kb") => (&lower[..lower.len() - 2], 1024),
        _ if lower.ends_with("mb") => (&lower[..lower.len() - 2], 1024 * 1024),
        _ if lower.ends_with("gb") => (&lower[..lower.len() - 2], 1024 * 1024 * 1024),
        _ if lower.ends_with('k') => (&lower[..lower.len() - 1], 1000),
        _ if lower.ends_with('m') => (&lower[..lower.len() - 1], 1_000_000),
        _ if lower.ends_with('g') => (&lower[..lower.len() - 1], 1_000_000_000),
        _ if lower.ends_with('b') => (&lower[..lower.len() - 1], 1),
        _ => (lower.as_str(), 1),
    };
    digits.parse::<u64>().ok()?.checked_mul(mult)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_redis_shapes() {
        let cfg = ConfigStore::default();
        assert_eq!(cfg.get("maxmemory"), Some("0"));
        assert_eq!(cfg.get("maxmemory-policy"), Some("noeviction"));
        assert_eq!(cfg.get("databases"), Some("16"));
    }

    #[test]
    fn set_validates_and_normalizes() {
        let mut cfg = ConfigStore::default();
        assert_eq!(cfg.set(b"maxmemory", b"100mb"), Ok(ReloadClass::HotPerCell));
        assert_eq!(cfg.get("maxmemory"), Some("104857600"));
        assert_eq!(cfg.set(b"MAXMEMORY-POLICY", b"ALLKEYS-LFU"), Ok(ReloadClass::HotPerCell));
        assert_eq!(cfg.get("maxmemory-policy"), Some("allkeys-lfu"));
        assert!(matches!(
            cfg.set(b"maxmemory-policy", b"bogus"),
            Err(ConfigSetError::Invalid { .. })
        ));
        assert!(matches!(cfg.set(b"databases", b"32"), Err(ConfigSetError::Immutable(_))));
        assert!(matches!(cfg.set(b"nope", b"1"), Err(ConfigSetError::Unknown(_))));
    }

    #[test]
    fn glob_get_is_nocase_and_multi_pattern() {
        let cfg = ConfigStore::default();
        let hits = cfg.get_matching(&[b"MaxMemory*"]);
        assert_eq!(
            hits.iter().map(|(k, _)| *k).collect::<Vec<_>>(),
            vec!["maxmemory", "maxmemory-policy", "maxmemory-samples"]
        );
        let multi = cfg.get_matching(&[b"save", b"timeout"]);
        assert_eq!(multi.len(), 2);
    }
}
