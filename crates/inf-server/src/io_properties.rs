//! `io-properties.toml` — the device's probed barrier class (M4.5-S34,
//! ADR-0086 D7; the ScyllaDB `iotune` precedent).
//!
//! `inf probe-device <data-dir>` measures FLUSH-class (buffered +
//! `fdatasync`) and FUA-class (`O_DIRECT` + `O_DSYNC`) barrier latency on
//! the data directory's device and writes this file; `infinityd` reads it
//! at boot. **Absent ⇒ `Buffered`** — today's path byte-for-byte. The
//! format is a flat `key = value` subset of TOML (comments, blank lines,
//! bare integers, double-quoted strings) parsed here without a
//! dependency: the file is ours, and the measurement tools stay zero-dep.
//!
//! ```toml
//! # io-properties — written by `inf probe-device` (ADR-0086 D7)
//! barrier_class = "fua"            # "fua" | "flush"
//! fua_max_frame_bytes = 262144
//! fua_p50_us_4k = 294
//! flush_p50_us_4k = 915
//! probed_at_unix_s = 1755648000
//! # schema 2 (M4.5-S36, ADR-0088 D6): the device model the per-cell
//! # budget and the seal pacer spend. 0 / absent = not probed ⇒ that
//! # direction is unbudgeted (`io_budget_model:absent`).
//! probe_schema = 2
//! write_bytes_per_s_256k = 489000000
//! write_ops_per_s_4k = 2898
//! write_ops_per_s_4k_qd4 = 9918
//! read_bytes_per_s_256k = 0
//! read_ops_per_s_4k = 0
//! ```
//!
//! A malformed file is a typed boot refusal ([`IoPropertiesError`]) —
//! never a silent fallback to the slow class the operator did not choose.
//! Unknown keys are ignored (a schema-1 binary reads a schema-2 file;
//! a schema-2 binary reads a schema-1 file with the model absent and
//! the boot log says so).

use std::fmt;
use std::path::Path;

use inf_log::{DEFAULT_FUA_MAX_FRAME_BYTES, FRAME_ALIGN, SegmentIoMode};
use inf_runtime::DeviceModel;

/// File name under the data directory.
pub const IO_PROPERTIES_FILE: &str = "io-properties.toml";

/// The probed device properties the log writer consumes.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct IoProperties {
    /// The segment I/O mode the probe recommends.
    pub io_mode: SegmentIoMode,
    /// Largest padded frame written write-through (`Direct` only).
    pub fua_max_frame_bytes: u32,
    /// Probed write-through p50 at 4 KiB (µs) — the tripwire reference
    /// (0 = not probed, tripwire disarmed).
    pub fua_p50_us_4k: u64,
    /// Probed buffered+fdatasync p50 at 4 KiB (µs), for the boot log line.
    pub flush_p50_us_4k: u64,
    /// Schema 2 (ADR-0088 D6): the probe file's schema (1 when absent —
    /// a pre-S36 file), the device model (per device; the cell share is
    /// computed at boot), and the concurrent barrier rate the seal pacer
    /// refills from (`write_ops_per_s_4k_qd4`; 0 = unpaced).
    pub probe_schema: u64,
    pub device: DeviceModel,
    pub write_ops_per_s_4k_qd4: u64,
}

impl Default for IoProperties {
    /// The absent-file default: today's FLUSH class, no device model.
    fn default() -> IoProperties {
        IoProperties {
            io_mode: SegmentIoMode::Buffered,
            fua_max_frame_bytes: DEFAULT_FUA_MAX_FRAME_BYTES,
            fua_p50_us_4k: 0,
            flush_p50_us_4k: 0,
            probe_schema: 1,
            device: DeviceModel::ABSENT,
            write_ops_per_s_4k_qd4: 0,
        }
    }
}

/// Why an `io-properties.toml` was refused.
#[derive(Debug)]
pub enum IoPropertiesError {
    Io(std::io::Error),
    /// `line` (1-based) is not `key = value`, or `key` repeats.
    Syntax {
        line: usize,
        detail: &'static str,
    },
    /// A known key carries a value of the wrong shape or range.
    Value {
        key: &'static str,
        detail: String,
    },
    /// `barrier_class` is missing.
    MissingClass,
}

impl fmt::Display for IoPropertiesError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IoPropertiesError::Io(err) => write!(f, "io-properties: {err}"),
            IoPropertiesError::Syntax { line, detail } => {
                write!(f, "io-properties line {line}: {detail}")
            }
            IoPropertiesError::Value { key, detail } => {
                write!(f, "io-properties `{key}`: {detail}")
            }
            IoPropertiesError::MissingClass => write!(f, "io-properties: `barrier_class` missing"),
        }
    }
}

impl std::error::Error for IoPropertiesError {}

impl IoProperties {
    /// Read `<data_dir>/io-properties.toml`; `Ok(None)` when absent.
    ///
    /// # Errors
    /// A present-but-malformed file (never a silent default).
    pub fn load(data_dir: &Path) -> Result<Option<IoProperties>, IoPropertiesError> {
        let path = data_dir.join(IO_PROPERTIES_FILE);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(IoPropertiesError::Io(err)),
        };
        IoProperties::parse(&text).map(Some)
    }

    /// Parse the file's text.
    ///
    /// # Errors
    /// Syntax, value-shape, or missing-class failures.
    pub fn parse(text: &str) -> Result<IoProperties, IoPropertiesError> {
        let mut class = None;
        let mut props = IoProperties::default();
        let mut seen: Vec<&str> = Vec::new();
        for (index, raw) in text.lines().enumerate() {
            let line = index + 1;
            let content = raw.split('#').next().unwrap_or("").trim();
            if content.is_empty() {
                continue;
            }
            let Some((key, value)) = content.split_once('=') else {
                return Err(IoPropertiesError::Syntax { line, detail: "expected `key = value`" });
            };
            let (key, value) = (key.trim(), value.trim());
            if seen.contains(&key) {
                return Err(IoPropertiesError::Syntax { line, detail: "duplicate key" });
            }
            seen.push(key);
            match key {
                "barrier_class" => class = Some(parse_class(value)?),
                "fua_max_frame_bytes" => {
                    let bytes = parse_u64("fua_max_frame_bytes", value)?;
                    let bytes = u32::try_from(bytes)
                        .ok()
                        .filter(|b| *b >= FRAME_ALIGN && b.is_multiple_of(FRAME_ALIGN));
                    props.fua_max_frame_bytes = bytes.ok_or_else(|| IoPropertiesError::Value {
                        key: "fua_max_frame_bytes",
                        detail: format!("must be a multiple of {FRAME_ALIGN} that fits u32"),
                    })?;
                }
                "fua_p50_us_4k" => props.fua_p50_us_4k = parse_u64("fua_p50_us_4k", value)?,
                "flush_p50_us_4k" => props.flush_p50_us_4k = parse_u64("flush_p50_us_4k", value)?,
                // Schema 2 (ADR-0088 D6) — each typed, none required.
                "probe_schema" => props.probe_schema = parse_u64("probe_schema", value)?,
                "write_bytes_per_s_256k" => {
                    props.device.write_bytes_per_s = parse_u64("write_bytes_per_s_256k", value)?;
                }
                "write_ops_per_s_4k" => {
                    props.device.write_ops_per_s = parse_u64("write_ops_per_s_4k", value)?;
                }
                "write_ops_per_s_4k_qd4" => {
                    props.write_ops_per_s_4k_qd4 = parse_u64("write_ops_per_s_4k_qd4", value)?;
                }
                "read_bytes_per_s_256k" => {
                    props.device.read_bytes_per_s = parse_u64("read_bytes_per_s_256k", value)?;
                }
                "read_ops_per_s_4k" => {
                    props.device.read_ops_per_s = parse_u64("read_ops_per_s_4k", value)?;
                }
                // Provenance keys are informational; unknown keys are a
                // forward-compatibility allowance, not an error.
                _ => {}
            }
        }
        props.io_mode = class.ok_or(IoPropertiesError::MissingClass)?;
        Ok(props)
    }
}

fn parse_class(value: &str) -> Result<SegmentIoMode, IoPropertiesError> {
    match value.trim_matches('"') {
        "fua" => Ok(SegmentIoMode::Direct),
        "flush" => Ok(SegmentIoMode::Buffered),
        other => Err(IoPropertiesError::Value {
            key: "barrier_class",
            detail: format!("expected \"fua\" or \"flush\", found {other:?}"),
        }),
    }
}

fn parse_u64(key: &'static str, value: &str) -> Result<u64, IoPropertiesError> {
    value.replace('_', "").parse::<u64>().map_err(|_| IoPropertiesError::Value {
        key,
        detail: format!("expected an integer, found {value:?}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_probe_output_shape() {
        // The file `inf probe-device` writes: comments, quoted class,
        // bare integers, a provenance key the reader ignores.
        let text = "# io-properties — written by `inf probe-device`\n\
                    barrier_class = \"fua\"   # \"fua\" | \"flush\"\n\
                    fua_max_frame_bytes = 262144\n\
                    fua_p50_us_4k = 294\n\
                    flush_p50_us_4k = 915\n\
                    probed_at_unix_s = 1755648000\n";
        let props = IoProperties::parse(text).expect("valid");
        assert_eq!(props.io_mode, SegmentIoMode::Direct);
        assert_eq!(props.fua_max_frame_bytes, 262_144);
        assert_eq!(props.fua_p50_us_4k, 294);
        assert_eq!(props.flush_p50_us_4k, 915);
        // A schema-1 file: the model is absent and says so (ADR-0088 D6).
        assert_eq!(props.probe_schema, 1);
        assert!(props.device.is_absent());
        assert_eq!(props.write_ops_per_s_4k_qd4, 0);
    }

    /// ADR-0088 D6: schema 2 carries the device model; a zero or absent
    /// direction stays unbudgeted; a malformed value is a typed refusal.
    #[test]
    fn schema_2_carries_the_device_model() {
        let text = "barrier_class = \"fua\"\n\
                    probe_schema = 2\n\
                    write_bytes_per_s_256k = 489_000_000\n\
                    write_ops_per_s_4k = 2898\n\
                    write_ops_per_s_4k_qd4 = 9_918\n\
                    read_bytes_per_s_256k = 0\n";
        let props = IoProperties::parse(text).expect("valid");
        assert_eq!(props.probe_schema, 2);
        assert_eq!(props.device.write_bytes_per_s, 489_000_000);
        assert_eq!(props.device.write_ops_per_s, 2_898);
        assert_eq!(props.write_ops_per_s_4k_qd4, 9_918);
        assert_eq!(props.device.read_bytes_per_s, 0);
        assert_eq!(props.device.read_ops_per_s, 0);
        assert!(!props.device.is_absent());
        assert!(matches!(
            IoProperties::parse("barrier_class = \"fua\"\nwrite_bytes_per_s_256k = fast\n"),
            Err(IoPropertiesError::Value { key: "write_bytes_per_s_256k", .. })
        ));
    }

    #[test]
    fn flush_class_keeps_the_buffered_default() {
        let props = IoProperties::parse("barrier_class = \"flush\"\n").expect("valid");
        assert_eq!(props.io_mode, SegmentIoMode::Buffered);
        assert_eq!(props.fua_max_frame_bytes, DEFAULT_FUA_MAX_FRAME_BYTES);
    }

    #[test]
    fn malformed_files_are_refused_not_defaulted() {
        // A present file the operator wrote must never silently become
        // the slow class: every malformation is a typed refusal.
        assert!(matches!(IoProperties::parse(""), Err(IoPropertiesError::MissingClass)));
        assert!(matches!(
            IoProperties::parse("barrier_class = \"turbo\""),
            Err(IoPropertiesError::Value { key: "barrier_class", .. })
        ));
        assert!(matches!(
            IoProperties::parse("barrier_class = \"fua\"\nfua_max_frame_bytes = 4097"),
            Err(IoPropertiesError::Value { key: "fua_max_frame_bytes", .. })
        ));
        assert!(matches!(
            IoProperties::parse("barrier_class \"fua\""),
            Err(IoPropertiesError::Syntax { line: 1, .. })
        ));
        assert!(matches!(
            IoProperties::parse("barrier_class = \"fua\"\nbarrier_class = \"flush\""),
            Err(IoPropertiesError::Syntax { line: 2, .. })
        ));
    }

    #[test]
    fn absent_file_is_none() {
        let dir = std::env::temp_dir().join(format!("inf-ioprops-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("tmp dir");
        assert!(IoProperties::load(&dir).expect("absent is fine").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
