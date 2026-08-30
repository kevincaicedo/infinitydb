//! The data directory's key-hash secret (ADR-0094 D2): `key-hash.toml`.
//!
//! Every index a data directory persists — the checkpoint's `(hash,
//! addr)` refs, the tiered sidecar's 64-bit hashes, the ADR-0076
//! primary-key refs — is a function of the node's [`KeyHasher`], so the
//! secret is written at the directory's **first boot**, before the
//! catalog and before any cell creates a log, and never changes for the
//! directory's life. A directory that already holds data without the
//! file predates the ADR and is refused (no migration: re-deriving every
//! ref would read every cold record — the §3.3 boot rule; the product
//! is pre-1.0). Boot code: blocking file I/O is fine here; no cell runs
//! yet.
//!
//! The first amendment (ADR-0094 D6–D9) adds what D2 lacked: the secret's
//! **identity** (`KeyHasher::identity`) is named by every `MANIFEST`
//! (epoch 3), and [`verify_key_hash_binding`] refuses a directory whose
//! checkpoints were placed under another secret — before any cell
//! starts; the file is created **private** (`0600`, checked on the handle
//! every load — a lax mode is a refusal naming the `chmod`); publication
//! is **create-exclusive** (`create_new` temp, `link(2)` no-replace);
//! and the caller holds the directory's owner lock
//! (`crate::data_dir_lock`) around all of it.
//!
//! Format (one `key = value` per line, `#` comments):
//!
//! ```text
//! schema = 1
//! function = "siphash13"
//! k0 = 0x…            # 16 hex digits
//! k1 = 0x…
//! ```

use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use inf_foundation::{KeyHashId, KeyHasher};
use inf_log::fs::StdSegmentFs;
use inf_log::read_manifest;

/// The file's name under the data directory.
pub const KEY_HASH_FILE: &str = "key-hash.toml";
/// The catalog file whose presence marks a directory that already holds
/// data (ADR-0015 D3 — the control thread's `META`).
const CATALOG_FILE: &str = "META";
const SCHEMA: u64 = 1;
const FUNCTION: &str = "siphash13";

/// Why the secret could not be resolved — each a boot refusal (fail-stop
/// with the reason named, never a fixed fallback).
#[derive(Debug)]
pub enum KeyHashError {
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
    /// The file names a schema or function this binary does not carry.
    Unsupported(String),
    /// The directory holds a catalog or cell data but no secret: it
    /// predates ADR-0094.
    Predates,
    /// The OS gave no entropy for a first boot.
    Entropy(io::Error),
    /// The file is readable by its group or others (ADR-0094 D9): a
    /// world-readable secret reopens the hash-flooding threat.
    Permissions {
        mode: u32,
    },
    /// Publication found a secret already in place (ADR-0094 D8): the
    /// directory's owner lock was not held — fail-stop, never "use
    /// whichever won".
    Contended,
    /// A shard's `MANIFEST` names a secret other than the one this
    /// directory holds (ADR-0094 D6): the secret was replaced after that
    /// checkpoint was placed.
    Mismatch {
        shard: String,
        manifest: KeyHashId,
        secret: KeyHashId,
    },
    /// A shard's `MANIFEST` could not be read for the binding scan (a
    /// legacy epoch that cannot name its secret, or corruption).
    Manifest {
        shard: String,
        detail: String,
    },
}

impl fmt::Display for KeyHashError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KeyHashError::Io(err) => write!(f, "{KEY_HASH_FILE}: {err}"),
            KeyHashError::Syntax { line, detail } => {
                write!(f, "{KEY_HASH_FILE}:{line}: {detail}")
            }
            KeyHashError::Value { key, detail } => write!(f, "{KEY_HASH_FILE} `{key}`: {detail}"),
            KeyHashError::Missing(key) => write!(f, "{KEY_HASH_FILE}: `{key}` missing"),
            KeyHashError::Unsupported(what) => {
                write!(f, "{KEY_HASH_FILE}: {what} is not one this binary carries")
            }
            KeyHashError::Predates => write!(
                f,
                "the data directory holds a catalog or cell data but no {KEY_HASH_FILE}: it \
                 predates keyed key hashing (ADR-0094) and cannot be opened by this binary — \
                 its checkpoint refs were placed under the old fixed hash. Reload from a dump \
                 into a new directory (fail-stop)"
            ),
            KeyHashError::Entropy(err) => {
                write!(f, "no entropy for the first boot's key-hash secret: {err}")
            }
            KeyHashError::Permissions { mode } => write!(
                f,
                "{KEY_HASH_FILE} is mode {mode:04o}: the secret must be readable by its owner \
                 only (ADR-0094 D9) — `chmod 600` it (fail-stop)"
            ),
            KeyHashError::Contended => write!(
                f,
                "{KEY_HASH_FILE} appeared while this first boot was publishing its own: the data \
                 directory has a second writer (ADR-0094 D7/D8) — fail-stop"
            ),
            KeyHashError::Mismatch { shard, manifest, secret } => write!(
                f,
                "{shard}/MANIFEST names key-hash id {manifest} but {KEY_HASH_FILE} holds \
                 {secret}: the secret was replaced after that checkpoint was placed (ADR-0094 \
                 D6) — every cold ref would be silently unreachable; restore the directory's \
                 original {KEY_HASH_FILE} (fail-stop)"
            ),
            KeyHashError::Manifest { shard, detail } => write!(f, "{shard}/MANIFEST: {detail}"),
        }
    }
}

impl std::error::Error for KeyHashError {}

impl From<io::Error> for KeyHashError {
    fn from(err: io::Error) -> KeyHashError {
        KeyHashError::Io(err)
    }
}

/// Where a boot's secret came from (the boot line says so).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum KeyHashSource {
    /// Read from an existing `key-hash.toml`.
    File,
    /// Drawn from the OS and written now — the directory's first boot.
    Created,
}

/// Read `<data_dir>/key-hash.toml`; `Ok(None)` when absent. The mode is
/// checked on the handle the text is then read from (ADR-0094 D9): no
/// stat-then-read window.
///
/// # Errors
/// A present-but-malformed file, or one readable beyond its owner
/// (never a silent default).
pub fn load_key_hash(data_dir: &Path) -> Result<Option<KeyHasher>, KeyHashError> {
    let mut file = match File::open(data_dir.join(KEY_HASH_FILE)) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(KeyHashError::Io(err)),
    };
    require_private(&file)?;
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    parse_key_hash(&text).map(Some)
}

/// ADR-0094 D9: no group/other permission bit on the secret. Unix only —
/// the product's targets (Linux, the macOS dev tier); elsewhere the mode
/// model does not exist and the check is vacuous.
fn require_private(file: &File) -> Result<(), KeyHashError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = file.metadata()?.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            return Err(KeyHashError::Permissions { mode });
        }
    }
    #[cfg(not(unix))]
    let _ = file;
    Ok(())
}

/// Parse the file's text.
///
/// # Errors
/// Syntax, value-shape, missing-key, or unsupported-version failures.
pub fn parse_key_hash(text: &str) -> Result<KeyHasher, KeyHashError> {
    let (mut schema, mut function, mut k0, mut k1) = (None, None, None, None);
    let mut seen: Vec<&str> = Vec::new();
    for (index, raw) in text.lines().enumerate() {
        let line = index + 1;
        let content = raw.split('#').next().unwrap_or("").trim();
        if content.is_empty() {
            continue;
        }
        let Some((key, value)) = content.split_once('=') else {
            return Err(KeyHashError::Syntax { line, detail: "expected `key = value`" });
        };
        let (key, value) = (key.trim(), value.trim());
        if seen.contains(&key) {
            return Err(KeyHashError::Syntax { line, detail: "duplicate key" });
        }
        seen.push(key);
        match key {
            "schema" => schema = Some(parse_u64("schema", value)?),
            "function" => {
                let Some(name) = value.strip_prefix('"').and_then(|v| v.strip_suffix('"')) else {
                    return Err(KeyHashError::Value {
                        key: "function",
                        detail: "expected a quoted name".to_owned(),
                    });
                };
                function = Some(name.to_owned());
            }
            "k0" => k0 = Some(parse_u64("k0", value)?),
            "k1" => k1 = Some(parse_u64("k1", value)?),
            _ => return Err(KeyHashError::Syntax { line, detail: "unknown key" }),
        }
    }
    let schema = schema.ok_or(KeyHashError::Missing("schema"))?;
    if schema != SCHEMA {
        return Err(KeyHashError::Unsupported(format!("schema {schema}")));
    }
    let function = function.ok_or(KeyHashError::Missing("function"))?;
    if function != FUNCTION {
        return Err(KeyHashError::Unsupported(format!("function `{function}`")));
    }
    let k0 = k0.ok_or(KeyHashError::Missing("k0"))?;
    let k1 = k1.ok_or(KeyHashError::Missing("k1"))?;
    Ok(KeyHasher::from_keys(k0, k1))
}

fn parse_u64(key: &'static str, value: &str) -> Result<u64, KeyHashError> {
    let parsed = match value.strip_prefix("0x") {
        Some(hex) => u64::from_str_radix(hex, 16),
        None => value.parse::<u64>(),
    };
    parsed.map_err(|_| KeyHashError::Value {
        key,
        detail: format!("expected an unsigned 64-bit integer, got `{value}`"),
    })
}

/// The file's text for `hasher` — what a first boot writes.
#[must_use]
pub fn render_key_hash(hasher: KeyHasher) -> String {
    let (k0, k1) = hasher.keys();
    format!(
        "# InfinityDB key-hash secret (ADR-0094). Written once at this data\n\
         # directory's first boot; every checkpoint ref and index sidecar under\n\
         # it is placed by this secret. Never edit, never copy between\n\
         # directories.\n\
         schema = {SCHEMA}\n\
         function = \"{FUNCTION}\"\n\
         k0 = {k0:#018x}\n\
         k1 = {k1:#018x}\n"
    )
}

/// Whether `data_dir` already holds a catalog or any cell's shard
/// directory — the mark of a directory that has booted before.
///
/// # Errors
/// The directory cannot be listed (a missing directory is `false`).
pub fn directory_has_data(data_dir: &Path) -> io::Result<bool> {
    if data_dir.join(CATALOG_FILE).exists() {
        return Ok(true);
    }
    let entries = match std::fs::read_dir(data_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err),
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        if name.to_string_lossy().starts_with("shard-") && entry.file_type()?.is_dir() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Write `<data_dir>/key-hash.toml` durably and exclusively (ADR-0094
/// D8/D9): a `create_new` temp file at mode `0600` (a stale temp from a
/// crashed earlier first boot is removed first — under the owner lock
/// nobody else can be writing it), `fsync`, then `link(2)` onto the
/// final name — atomic and **no-replace** by the kernel's contract —
/// the temp unlinked, and the directory `fsync`. The directory is
/// created if absent.
///
/// # Errors
/// [`KeyHashError::Contended`] when a secret is already in place (the
/// owner lock was not held); [`KeyHashError::Permissions`] when the
/// created file is not private; any I/O failure (the boot refuses; a
/// half-written secret must never be read back as one).
pub fn create_key_hash(data_dir: &Path, hasher: KeyHasher) -> Result<(), KeyHashError> {
    std::fs::create_dir_all(data_dir)?;
    let tmp = data_dir.join(format!("{KEY_HASH_FILE}.tmp"));
    let target = data_dir.join(KEY_HASH_FILE);
    match std::fs::remove_file(&tmp) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(KeyHashError::Io(err)),
    }
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    {
        let mut file = options.open(&tmp)?;
        require_private(&file)?;
        file.write_all(render_key_hash(hasher).as_bytes())?;
        file.sync_all()?;
    }
    // Publication: `link` never replaces — a present secret is EEXIST.
    match std::fs::hard_link(&tmp, &target) {
        Ok(()) => {}
        Err(err) => {
            let _ = std::fs::remove_file(&tmp);
            return Err(if err.kind() == io::ErrorKind::AlreadyExists {
                KeyHashError::Contended
            } else {
                KeyHashError::Io(err)
            });
        }
    }
    std::fs::remove_file(&tmp)?;
    File::open(data_dir)?.sync_all()?;
    Ok(())
}

/// What the binding pre-scan found (the boot line says so).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct KeyHashBinding {
    /// Shards whose `MANIFEST` names this directory's secret.
    pub manifests_bound: u32,
    /// Shards that have never published a checkpoint (nothing to bind).
    pub shards_unpublished: u32,
}

/// ADR-0094 D6, node level: every `shard-*/MANIFEST` must name the
/// identity of `hasher` — run after the secret resolves and **before**
/// the catalog, the device profile, or any cell thread, so a mismatched
/// directory is left byte-identical (no cell can publish a manifest
/// under the wrong secret while another refuses). Shards are visited in
/// name order; the first mismatch is the refusal.
///
/// # Errors
/// [`KeyHashError::Mismatch`] naming the shard and both identities;
/// [`KeyHashError::Manifest`] when a manifest cannot be read (a legacy
/// epoch that cannot name its secret, or corruption); I/O failures.
pub fn verify_key_hash_binding(
    data_dir: &Path,
    hasher: KeyHasher,
) -> Result<KeyHashBinding, KeyHashError> {
    let secret = hasher.identity();
    let mut report = KeyHashBinding::default();
    let mut shards: Vec<PathBuf> = Vec::new();
    let entries = match std::fs::read_dir(data_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(report),
        Err(err) => return Err(KeyHashError::Io(err)),
    };
    for entry in entries {
        let entry = entry?;
        if entry.file_name().to_string_lossy().starts_with("shard-") && entry.file_type()?.is_dir()
        {
            shards.push(entry.path());
        }
    }
    shards.sort();
    for shard in shards {
        let name = shard.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        match read_manifest(&StdSegmentFs, &shard) {
            Ok(Some(manifest)) if manifest.key_hash_id == secret => report.manifests_bound += 1,
            Ok(Some(manifest)) => {
                return Err(KeyHashError::Mismatch {
                    shard: name,
                    manifest: manifest.key_hash_id,
                    secret,
                });
            }
            Ok(None) => report.shards_unpublished += 1,
            Err(err) => {
                return Err(KeyHashError::Manifest { shard: name, detail: err.to_string() });
            }
        }
    }
    Ok(report)
}

/// Load → or, on a directory that has never booted, draw from `entropy`
/// and create. A directory with data but no file is refused.
///
/// # Errors
/// [`KeyHashError`] — every variant is a boot refusal.
pub fn resolve_key_hash(
    data_dir: &Path,
    entropy: impl FnOnce() -> io::Result<[u8; 16]>,
) -> Result<(KeyHasher, KeyHashSource), KeyHashError> {
    if let Some(hasher) = load_key_hash(data_dir)? {
        return Ok((hasher, KeyHashSource::File));
    }
    if directory_has_data(data_dir)? {
        return Err(KeyHashError::Predates);
    }
    let bytes = entropy().map_err(KeyHashError::Entropy)?;
    let k0 = u64::from_le_bytes(bytes[..8].try_into().expect("8 bytes"));
    let k1 = u64::from_le_bytes(bytes[8..].try_into().expect("8 bytes"));
    let hasher = KeyHasher::from_keys(k0, k1);
    create_key_hash(data_dir, hasher)?;
    Ok((hasher, KeyHashSource::Created))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "inf-key-hash-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn entropy() -> io::Result<[u8; 16]> {
        Ok([0x5A; 16])
    }

    /// ADR-0094 K2: the first boot creates the file from entropy; the
    /// second boot reads the same secret back.
    #[test]
    fn first_boot_creates_and_the_next_boot_reads_the_same_secret() {
        let dir = fresh_dir("roundtrip");
        let (created, source) = resolve_key_hash(&dir, entropy).expect("first boot");
        assert_eq!(source, KeyHashSource::Created);
        assert_eq!(created.keys(), (0x5A5A_5A5A_5A5A_5A5A, 0x5A5A_5A5A_5A5A_5A5A));
        let (loaded, source) =
            resolve_key_hash(&dir, || panic!("no entropy on a second boot")).expect("second");
        assert_eq!(source, KeyHashSource::File);
        assert_eq!(loaded, created);
        assert!(!dir.join(format!("{KEY_HASH_FILE}.tmp")).exists(), "no temp residue");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ADR-0094 D2: a directory that holds data without the file is
    /// refused, whether the mark is the catalog or a shard directory.
    #[test]
    fn a_pre_adr_directory_is_refused() {
        for mark in ["META", "shard-0"] {
            let dir = fresh_dir(&format!("predates-{mark}"));
            std::fs::create_dir_all(&dir).expect("dir");
            if mark == "META" {
                std::fs::write(dir.join(mark), b"catalog").expect("mark");
            } else {
                std::fs::create_dir(dir.join(mark)).expect("mark");
            }
            let err = resolve_key_hash(&dir, || panic!("no entropy for a refused boot"))
                .expect_err("refused");
            assert!(matches!(err, KeyHashError::Predates), "{mark}: {err}");
            assert!(err.to_string().contains("ADR-0094"));
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// A malformed or foreign file is a refusal with the line named —
    /// never a fresh secret over existing refs.
    #[test]
    fn malformed_and_unsupported_files_are_refused() {
        let cases: [(&str, &str); 6] = [
            ("schema = 1\nfunction = \"siphash13\"\nk0 = 0x1\n", "`k1` missing"),
            ("schema = 2\nfunction = \"siphash13\"\nk0 = 1\nk1 = 2\n", "schema 2"),
            ("schema = 1\nfunction = \"wyhash\"\nk0 = 1\nk1 = 2\n", "function `wyhash`"),
            ("schema = 1\nfunction = \"siphash13\"\nk0 = zz\nk1 = 2\n", "`k0`"),
            ("schema = 1\nfunction = \"siphash13\"\nk0 = 1\nk0 = 1\nk1 = 2\n", ":4: duplicate"),
            ("schema = 1\nbogus\n", ":2: expected"),
        ];
        for (text, needle) in cases {
            let err = parse_key_hash(text).expect_err(text);
            assert!(err.to_string().contains(needle), "{text:?} → {err}");
        }
        let dir = fresh_dir("malformed");
        std::fs::create_dir_all(&dir).expect("dir");
        std::fs::write(dir.join(KEY_HASH_FILE), "schema = 1\n").expect("write");
        assert!(resolve_key_hash(&dir, entropy).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ADR-0094 D9: the first boot's file is `0600`; a file readable
    /// beyond its owner is refused naming the mode and the `chmod`, and
    /// boots again once private.
    #[cfg(unix)]
    #[test]
    fn the_secret_is_private_and_a_lax_mode_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let dir = fresh_dir("private");
        let (hasher, _) = resolve_key_hash(&dir, entropy).expect("first boot");
        let file = dir.join(KEY_HASH_FILE);
        let mode = std::fs::metadata(&file).expect("meta").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "{mode:04o}");
        for lax in [0o644u32, 0o640, 0o604, 0o660] {
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(lax)).expect("chmod");
            let err = resolve_key_hash(&dir, || panic!("no entropy")).expect_err("refused");
            assert!(matches!(err, KeyHashError::Permissions { mode } if mode == lax), "{err}");
            assert!(err.to_string().contains("chmod 600"), "{err}");
        }
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o400)).expect("chmod");
        let (again, source) =
            resolve_key_hash(&dir, || panic!("no entropy")).expect("0400 is private");
        assert_eq!((again, source), (hasher, KeyHashSource::File));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ADR-0094 D8: publication never replaces a secret already in
    /// place — the typed `Contended`, the original bytes untouched, no
    /// temp residue; and a stale temp from a crashed first boot is
    /// cleared, not read.
    #[test]
    fn publication_is_exclusive_and_clears_a_stale_temp() {
        let dir = fresh_dir("exclusive");
        let tmp = dir.join(format!("{KEY_HASH_FILE}.tmp"));
        std::fs::create_dir_all(&dir).expect("dir");
        std::fs::write(&tmp, b"garbage from a crashed first boot").expect("stale temp");
        create_key_hash(&dir, KeyHasher::from_keys(1, 2)).expect("first publication");
        assert!(!tmp.exists(), "the stale temp is gone");
        let first = std::fs::read_to_string(dir.join(KEY_HASH_FILE)).expect("published");
        assert_eq!(parse_key_hash(&first).expect("parses").keys(), (1, 2));
        let err = create_key_hash(&dir, KeyHasher::from_keys(3, 4)).expect_err("no replace");
        assert!(matches!(err, KeyHashError::Contended), "{err}");
        assert!(err.to_string().contains("second writer"), "{err}");
        assert_eq!(std::fs::read_to_string(dir.join(KEY_HASH_FILE)).expect("still"), first);
        assert!(!tmp.exists(), "the loser's temp is unlinked");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ADR-0094 D6, the node-level pre-scan: a shard whose MANIFEST names
    /// another secret is the typed `Mismatch` (shard and both ids named);
    /// a shard without a manifest is merely unpublished; a legacy-epoch
    /// manifest is refused through the decoder's own reason.
    #[test]
    fn the_binding_scan_refuses_a_replaced_secret_and_a_legacy_manifest() {
        use inf_log::fs::StdSegmentFs;
        use inf_log::{Lsn, Manifest, SegmentId, write_manifest};
        let dir = fresh_dir("binding");
        let placed = KeyHasher::from_keys(11, 12);
        let other = KeyHasher::from_keys(21, 22);
        for shard in ["shard-0", "shard-1"] {
            std::fs::create_dir_all(dir.join(shard)).expect("shard");
        }
        write_manifest(
            &StdSegmentFs,
            &dir.join("shard-0"),
            &Manifest::tierless(
                1,
                Lsn::new(SegmentId(0), 64),
                vec![SegmentId(0)],
                placed.identity(),
            ),
        )
        .expect("manifest");
        assert_eq!(
            verify_key_hash_binding(&dir, placed).expect("bound"),
            KeyHashBinding { manifests_bound: 1, shards_unpublished: 1 }
        );
        let err = verify_key_hash_binding(&dir, other).expect_err("replaced secret");
        match &err {
            KeyHashError::Mismatch { shard, manifest, secret } => {
                assert_eq!(shard, "shard-0");
                assert_eq!((*manifest, *secret), (placed.identity(), other.identity()));
            }
            other => panic!("{other}"),
        }
        assert!(err.to_string().contains("ADR-0094") && err.to_string().contains("fail-stop"));
        // A legacy (epoch-1) manifest cannot name its secret: refused by
        // the decoder's reason, relayed with the shard named.
        let mut legacy = Manifest::tierless(
            1,
            Lsn::new(SegmentId(0), 64),
            vec![SegmentId(0)],
            placed.identity(),
        )
        .encode();
        legacy[8..12].copy_from_slice(&1u32.to_le_bytes());
        std::fs::create_dir_all(dir.join("shard-2")).expect("shard");
        inf_log::meta::write_envelope(
            &StdSegmentFs,
            &dir.join("shard-2"),
            "MANIFEST.new",
            "MANIFEST",
            &legacy,
        )
        .expect("legacy manifest");
        let err = verify_key_hash_binding(&dir, placed).expect_err("legacy refused");
        assert!(
            matches!(&err, KeyHashError::Manifest { shard, .. } if shard == "shard-2"),
            "{err}"
        );
        assert!(err.to_string().contains("predates the key-hash binding"), "{err}");
        // An absent directory has nothing to bind.
        assert_eq!(
            verify_key_hash_binding(&fresh_dir("absent"), placed).expect("nothing"),
            KeyHashBinding::default()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The rendered file parses back to the same secret (hex, comments).
    #[test]
    fn render_parses_back() {
        let hasher = KeyHasher::from_keys(0x0123_4567_89AB_CDEF, 0xFEDC_BA98_7654_3210);
        let text = render_key_hash(hasher);
        assert!(text.contains("k0 = 0x0123456789abcdef"));
        assert_eq!(parse_key_hash(&text).expect("parses"), hasher);
        assert_eq!(
            parse_key_hash("schema=1\nfunction=\"siphash13\"\nk0=7\nk1=8").unwrap().keys(),
            (7, 8)
        );
    }
}
