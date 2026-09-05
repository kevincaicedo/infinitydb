//! The data directory's cell topology (ADR-0095): `topology.toml`.
//!
//! The 16,384-slot space is assigned to cells in contiguous ranges, so
//! the cell count is part of what the on-disk layout *means* — a
//! directory written at one count and reopened at another silently
//! loses access to most of its acked durable data (full-codebase review
//! of 2026-08-30, C8 / F-L14-03: 375 of 500 `FSYNC always` keys
//! unreachable after a `--cells 4` → `--cells 2` reopen that reported a
//! clean recovery). The count is therefore stamped at the directory's
//! first boot, under the ADR-0094 D7 owner lock and before any cell
//! starts, and a boot whose `--cells` disagrees is refused naming both
//! numbers (§8.4 — never a degraded serve).
//!
//! A directory that predates the file adopts by **derivation** (ADR-0095
//! D3): every completed boot creates `shard-0..cells-1` eagerly, so the
//! shard set is the recorded topology — a matching `--cells` stamps the
//! file, a different one is the same typed refusal, and a shard set
//! that is not exactly `{0..k-1}` is refused as underivable. Boot code:
//! blocking file I/O is fine here; no cell runs yet.
//!
//! Format (one `key = value` per line, `#` comments):
//!
//! ```text
//! schema = 1
//! cells = 4
//! ```

use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::Path;

/// The file's name under the data directory.
pub const TOPOLOGY_FILE: &str = "topology.toml";
const SCHEMA: u64 = 1;
/// One cell per slot is the hard ceiling (`SLOT_COUNT`).
const MAX_CELLS: u64 = 16_384;

/// Why the topology could not be resolved — each a boot refusal
/// (fail-stop with the reason named, never a silent fallback).
#[derive(Debug)]
pub enum TopologyError {
    Io(io::Error),
    /// The file exists and is malformed at `line`.
    Syntax {
        line: usize,
        detail: &'static str,
    },
    /// A key's value is out of shape.
    Value {
        key: &'static str,
        detail: String,
    },
    /// A required key is absent.
    Missing(&'static str),
    /// The file names a schema this binary does not carry.
    Unsupported(String),
    /// The directory records one count and the boot asked for another
    /// (ADR-0095 D2) — the C8 refusal.
    Mismatch {
        recorded: u16,
        requested: u16,
    },
    /// No file, data present, and the shard set derives a count the
    /// boot's `--cells` contradicts (ADR-0095 D3).
    Derived {
        observed: u16,
        requested: u16,
    },
    /// No file, data present, and the shard set is not exactly
    /// `{0..k-1}` — the topology cannot be derived (a crashed pre-ADR
    /// first boot); the operator resolves.
    Underivable {
        detail: String,
    },
    /// Publication found a topology already in place (ADR-0094 D7/D8
    /// pattern): the directory's owner lock was not held — fail-stop.
    Contended,
}

impl fmt::Display for TopologyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TopologyError::Io(err) => write!(f, "{TOPOLOGY_FILE}: {err}"),
            TopologyError::Syntax { line, detail } => {
                write!(f, "{TOPOLOGY_FILE}:{line}: {detail}")
            }
            TopologyError::Value { key, detail } => write!(f, "{TOPOLOGY_FILE} `{key}`: {detail}"),
            TopologyError::Missing(key) => write!(f, "{TOPOLOGY_FILE}: `{key}` missing"),
            TopologyError::Unsupported(what) => {
                write!(f, "{TOPOLOGY_FILE}: {what} is not one this binary carries")
            }
            TopologyError::Mismatch { recorded, requested } => write!(
                f,
                "the data directory records a {recorded}-cell topology but this boot asked for \
                 {requested} (--cells): its keyspace is partitioned by the recorded count, and \
                 opening it at another silently loses access to acked durable data (ADR-0095, \
                 review C8). Reopen with --cells {recorded}; resizing is an explicit re-shard, \
                 not a flag edit (fail-stop)"
            ),
            TopologyError::Derived { observed, requested } => write!(
                f,
                "the data directory holds {observed} shard director{} but no {TOPOLOGY_FILE}, \
                 and this boot asked for {requested} cells (--cells): it was written at \
                 {observed} before topology binding (ADR-0095) and opening it at another count \
                 silently loses access to acked durable data (review C8). Reopen with --cells \
                 {observed} to adopt and stamp the topology (fail-stop)",
                if *observed == 1 { "y" } else { "ies" }
            ),
            TopologyError::Underivable { detail } => write!(
                f,
                "the data directory holds data but no {TOPOLOGY_FILE}, and its shard set does \
                 not derive a topology ({detail}): likely a crashed first boot that predates \
                 ADR-0095 — remove the partial directory or restore a complete one (fail-stop)"
            ),
            TopologyError::Contended => write!(
                f,
                "{TOPOLOGY_FILE} appeared while this first boot was publishing its own: the \
                 data directory has a second writer (ADR-0094 D7/D8) — fail-stop"
            ),
        }
    }
}

impl std::error::Error for TopologyError {}

impl From<io::Error> for TopologyError {
    fn from(err: io::Error) -> TopologyError {
        TopologyError::Io(err)
    }
}

/// Where a boot's topology came from (the boot line says so).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TopologySource {
    /// Read from an existing `topology.toml` and matched.
    File,
    /// Written now — the directory's first boot.
    Created,
    /// Derived from the pre-ADR directory's shard set, matched, and
    /// stamped (ADR-0095 D3).
    Adopted,
}

/// Read `<data_dir>/topology.toml`; `Ok(None)` when absent.
///
/// # Errors
/// A present-but-malformed file (never a silent default).
pub fn load_topology(data_dir: &Path) -> Result<Option<u16>, TopologyError> {
    let mut file = match File::open(data_dir.join(TOPOLOGY_FILE)) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(TopologyError::Io(err)),
    };
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    parse_topology(&text).map(Some)
}

/// Parse the file's text.
///
/// # Errors
/// Syntax, value-shape, missing-key, or unsupported-version failures.
pub fn parse_topology(text: &str) -> Result<u16, TopologyError> {
    let (mut schema, mut cells) = (None, None);
    let mut seen: Vec<&str> = Vec::new();
    for (index, raw) in text.lines().enumerate() {
        let line = index + 1;
        let content = raw.split('#').next().unwrap_or("").trim();
        if content.is_empty() {
            continue;
        }
        let Some((key, value)) = content.split_once('=') else {
            return Err(TopologyError::Syntax { line, detail: "expected `key = value`" });
        };
        let (key, value) = (key.trim(), value.trim());
        if seen.contains(&key) {
            return Err(TopologyError::Syntax { line, detail: "duplicate key" });
        }
        seen.push(key);
        match key {
            "schema" => schema = Some(parse_u64("schema", value)?),
            "cells" => cells = Some(parse_u64("cells", value)?),
            _ => return Err(TopologyError::Syntax { line, detail: "unknown key" }),
        }
    }
    let schema = schema.ok_or(TopologyError::Missing("schema"))?;
    if schema != SCHEMA {
        return Err(TopologyError::Unsupported(format!("schema {schema}")));
    }
    let cells = cells.ok_or(TopologyError::Missing("cells"))?;
    if cells == 0 || cells > MAX_CELLS {
        return Err(TopologyError::Value {
            key: "cells",
            detail: format!("expected 1..={MAX_CELLS}, got {cells}"),
        });
    }
    Ok(cells as u16)
}

fn parse_u64(key: &'static str, value: &str) -> Result<u64, TopologyError> {
    value.parse::<u64>().map_err(|_| TopologyError::Value {
        key,
        detail: format!("expected an unsigned integer, got `{value}`"),
    })
}

/// The file's text for `cells` — what a first boot writes.
#[must_use]
pub fn render_topology(cells: u16) -> String {
    format!(
        "# InfinityDB cell topology (ADR-0095). Written once at this data\n\
         # directory's first boot; the keyspace's slot ranges are partitioned\n\
         # by it. A boot whose --cells disagrees is refused — resizing is an\n\
         # explicit re-shard, never an edit of this file.\n\
         schema = {SCHEMA}\n\
         cells = {cells}\n"
    )
}

/// Write `<data_dir>/topology.toml` durably and exclusively (the
/// ADR-0094 D8 publication pattern: `create_new` temp, fsync, `link(2)`
/// no-replace, temp unlinked, directory fsync). The directory is
/// created if absent.
///
/// # Errors
/// [`TopologyError::Contended`] when a topology is already in place
/// (the owner lock was not held); any I/O failure.
pub fn create_topology(data_dir: &Path, cells: u16) -> Result<(), TopologyError> {
    std::fs::create_dir_all(data_dir)?;
    let tmp = data_dir.join(format!("{TOPOLOGY_FILE}.tmp"));
    let target = data_dir.join(TOPOLOGY_FILE);
    match std::fs::remove_file(&tmp) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(TopologyError::Io(err)),
    }
    {
        let mut file = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        file.write_all(render_topology(cells).as_bytes())?;
        file.sync_all()?;
    }
    // Publication: `link` never replaces — a present topology is EEXIST.
    match std::fs::hard_link(&tmp, &target) {
        Ok(()) => {}
        Err(err) => {
            let _ = std::fs::remove_file(&tmp);
            return Err(if err.kind() == io::ErrorKind::AlreadyExists {
                TopologyError::Contended
            } else {
                TopologyError::Io(err)
            });
        }
    }
    std::fs::remove_file(&tmp)?;
    File::open(data_dir)?.sync_all()?;
    Ok(())
}

/// The shard indices present under `data_dir`, or the reason they do
/// not derive a topology. Recovery creates `shard-0..cells-1` eagerly
/// before any cell serves, so for a completed pre-ADR boot the set is
/// exactly `{0..k-1}`.
fn derive_cells(data_dir: &Path) -> Result<Option<u16>, TopologyError> {
    let mut indices: Vec<u64> = Vec::new();
    let entries = match std::fs::read_dir(data_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(TopologyError::Io(err)),
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if let Some(index) = name.strip_prefix("shard-")
            && entry.file_type()?.is_dir()
        {
            match index.parse::<u64>() {
                Ok(index) => indices.push(index),
                Err(_) => {
                    return Err(TopologyError::Underivable {
                        detail: format!("`{name}` is not a numbered shard"),
                    });
                }
            }
        }
    }
    if indices.is_empty() {
        return Ok(None);
    }
    indices.sort_unstable();
    let contiguous = indices.iter().enumerate().all(|(i, &index)| index == i as u64);
    if !contiguous || indices.len() as u64 > MAX_CELLS {
        return Err(TopologyError::Underivable {
            detail: format!("shard indices {indices:?} are not exactly 0..k"),
        });
    }
    Ok(Some(indices.len() as u16))
}

/// Load → compare with `cells` → or, on a directory without the file,
/// derive from the shard set (ADR-0095 D3) or stamp a fresh first
/// boot's. Every disagreement is a typed boot refusal; the caller holds
/// the directory's owner lock.
///
/// # Errors
/// [`TopologyError`] — every variant is a boot refusal.
pub fn resolve_topology(data_dir: &Path, cells: u16) -> Result<TopologySource, TopologyError> {
    if let Some(recorded) = load_topology(data_dir)? {
        return if recorded == cells {
            Ok(TopologySource::File)
        } else {
            Err(TopologyError::Mismatch { recorded, requested: cells })
        };
    }
    match derive_cells(data_dir)? {
        Some(observed) if observed == cells => {
            create_topology(data_dir, cells)?;
            Ok(TopologySource::Adopted)
        }
        Some(observed) => Err(TopologyError::Derived { observed, requested: cells }),
        None => {
            create_topology(data_dir, cells)?;
            Ok(TopologySource::Created)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(
        clippy::disallowed_methods,
        reason = "test-only: a wall-clock stamp names the scratch dir"
    )]
    fn fresh_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "inf-topology-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// ADR-0095 K1: the first boot stamps the count; the next boot at
    /// the same count reads it back; a different count is the typed
    /// `Mismatch` naming both numbers, the file untouched.
    #[test]
    fn first_boot_stamps_and_a_mismatched_reopen_is_refused() {
        let dir = fresh_dir("roundtrip");
        assert_eq!(resolve_topology(&dir, 4).expect("first boot"), TopologySource::Created);
        assert_eq!(resolve_topology(&dir, 4).expect("second boot"), TopologySource::File);
        let before = std::fs::read_to_string(dir.join(TOPOLOGY_FILE)).expect("stamped");
        let err = resolve_topology(&dir, 2).expect_err("shrink refused");
        assert!(matches!(err, TopologyError::Mismatch { recorded: 4, requested: 2 }), "{err}");
        let text = err.to_string();
        assert!(text.contains('4') && text.contains('2') && text.contains("fail-stop"), "{text}");
        let err = resolve_topology(&dir, 8).expect_err("grow refused");
        assert!(matches!(err, TopologyError::Mismatch { recorded: 4, requested: 8 }), "{err}");
        assert_eq!(std::fs::read_to_string(dir.join(TOPOLOGY_FILE)).expect("kept"), before);
        assert!(!dir.join(format!("{TOPOLOGY_FILE}.tmp")).exists(), "no temp residue");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ADR-0095 K2: malformed or foreign files are refusals with the
    /// reason named — never a fresh stamp over an existing directory.
    #[test]
    fn malformed_and_unsupported_files_are_refused() {
        let cases: [(&str, &str); 7] = [
            ("schema = 1\n", "`cells` missing"),
            ("cells = 4\n", "`schema` missing"),
            ("schema = 2\ncells = 4\n", "schema 2"),
            ("schema = 1\ncells = 0\n", "expected 1..="),
            ("schema = 1\ncells = 99999\n", "expected 1..="),
            ("schema = 1\ncells = 4\ncells = 4\n", ":3: duplicate"),
            ("schema = 1\nbogus\n", ":2: expected"),
        ];
        for (text, needle) in cases {
            let err = parse_topology(text).expect_err(text);
            assert!(err.to_string().contains(needle), "{text:?} → {err}");
        }
        assert_eq!(parse_topology("schema = 1\ncells = 16384\n").expect("max"), 16_384);
        let dir = fresh_dir("malformed");
        std::fs::create_dir_all(&dir).expect("dir");
        std::fs::write(dir.join(TOPOLOGY_FILE), "schema = 1\n").expect("write");
        assert!(resolve_topology(&dir, 4).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ADR-0095 K3: a pre-ADR directory derives its topology from the
    /// shard set — a matching `--cells` adopts and stamps, a different
    /// one refuses naming both, and a non-contiguous set is
    /// underivable.
    #[test]
    fn a_pre_adr_directory_adopts_by_derivation_or_refuses() {
        let dir = fresh_dir("adopt");
        for shard in ["shard-0", "shard-1"] {
            std::fs::create_dir_all(dir.join(shard)).expect("shard");
        }
        let err = resolve_topology(&dir, 4).expect_err("derived mismatch");
        assert!(matches!(err, TopologyError::Derived { observed: 2, requested: 4 }), "{err}");
        assert!(err.to_string().contains("--cells 2"), "{err}");
        assert!(!dir.join(TOPOLOGY_FILE).exists(), "refusal leaves no stamp");
        assert_eq!(resolve_topology(&dir, 2).expect("adopt"), TopologySource::Adopted);
        assert_eq!(
            parse_topology(&std::fs::read_to_string(dir.join(TOPOLOGY_FILE)).expect("stamped"))
                .expect("parses"),
            2
        );
        assert_eq!(resolve_topology(&dir, 2).expect("next boot"), TopologySource::File);
        let gap = fresh_dir("gap");
        for shard in ["shard-0", "shard-2"] {
            std::fs::create_dir_all(gap.join(shard)).expect("shard");
        }
        let err = resolve_topology(&gap, 2).expect_err("underivable");
        assert!(matches!(err, TopologyError::Underivable { .. }), "{err}");
        assert!(err.to_string().contains("not exactly 0..k"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&gap);
    }

    /// The ADR-0094 D8 publication pattern holds: no replace, stale
    /// temps cleared, `Contended` when a second writer won.
    #[test]
    fn publication_is_exclusive_and_clears_a_stale_temp() {
        let dir = fresh_dir("exclusive");
        let tmp = dir.join(format!("{TOPOLOGY_FILE}.tmp"));
        std::fs::create_dir_all(&dir).expect("dir");
        std::fs::write(&tmp, b"garbage from a crashed first boot").expect("stale temp");
        create_topology(&dir, 4).expect("first publication");
        assert!(!tmp.exists(), "the stale temp is gone");
        let err = create_topology(&dir, 2).expect_err("no replace");
        assert!(matches!(err, TopologyError::Contended), "{err}");
        assert_eq!(
            parse_topology(&std::fs::read_to_string(dir.join(TOPOLOGY_FILE)).expect("kept"))
                .expect("parses"),
            4
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The rendered file parses back (comments included).
    #[test]
    fn render_parses_back() {
        for cells in [1u16, 4, 16_384] {
            assert_eq!(parse_topology(&render_topology(cells)).expect("parses"), cells);
        }
    }
}
