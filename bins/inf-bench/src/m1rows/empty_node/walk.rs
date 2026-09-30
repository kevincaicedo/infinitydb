//! The data directory's allocated bytes — Σ `st_blocks` × 512 over the
//! tree, each (dev, ino) once, symlinks never followed — and the dir
//! plant that proves the walk sees what it gates.

use std::collections::BTreeSet;
use std::io::Write as _;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};

use super::PlantVerdict;
use crate::gates::Gate;

/// The most entries (below the root) an empty node's tree may hold.
/// Crossing: `WalkError::OverCap` — this is not an empty node.
pub(super) const EMPTY_DIR_WALK_ENTRIES_MAX: usize = 4096;

/// Linux `st_blocks` counts 512-byte units.
const STAT_BLOCK_BYTES: u64 = 512;

/// One filesystem block of the bill: the dir row's resolution and the
/// plant's rounding unit.
pub(super) const DIR_STEP_BYTES: u64 = 4096;

/// The dir plant's smallest filler, bytes. No crossing (a floor): the
/// stopped directory can hold more than the bill (the stop checkpoint's
/// below-floor unlinks drain in slices after it, so a stop can exit before
/// they do; boot GC removes them), so the filler never derives from the
/// threshold alone.
pub(super) const DIR_PLANT_FILLER_MIN: u64 = 16 * 1024;

/// The dir plant's sparse file, bytes: `set_len` only, no byte written.
/// Fixed.
pub(super) const DIR_PLANT_SPARSE_BYTES: u64 = 1 << 30;

/// The filler generator's seed: fixed, so every run writes the same
/// non-repeating bytes and a compressing filesystem cannot shrink them.
const DIR_PLANT_FILLER_SEED: u64 = 0x0E4D_7D1E_F111_E500;

/// The files every node directory holds at the root (the bill's terms).
const NODE_ROOT_FILES: [&str; 4] = ["LOCK", "key-hash.toml", "topology.toml", "io-properties.toml"];

/// One complete walk of a node directory.
#[derive(Debug)]
pub(super) struct DirWalk {
    pub(super) allocated_bytes: u64,
    /// The largest `st_blksize` seen: the bill counts 4 KiB blocks.
    pub(super) block_bytes_max: u64,
    /// Every entry below the root, relative, with the bytes it added.
    pub(super) entries: Vec<(String, u64)>,
}

/// Why a walk produced no byte count.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum WalkError {
    Io {
        path: String,
        kind: std::io::ErrorKind,
    },
    /// More than [`EMPTY_DIR_WALK_ENTRIES_MAX`] entries.
    OverCap,
    /// A file every node directory holds is missing.
    NotANodeDir(String),
}

impl std::fmt::Display for WalkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WalkError::Io { path, kind } => write!(f, "walk: {path}: {kind}"),
            WalkError::OverCap => {
                write!(f, "walk: over {EMPTY_DIR_WALK_ENTRIES_MAX} entries: not an empty node")
            }
            WalkError::NotANodeDir(missing) => write!(f, "walk: not a node directory ({missing})"),
        }
    }
}

/// Walks `root` iteratively (an explicit stack, bounded by the entry cap)
/// and checks the skeleton of a `cells`-cell node directory.
pub(super) fn walk_allocated(root: &Path, cells: u16) -> Result<DirWalk, WalkError> {
    let mut walk = Walker { seen: BTreeSet::new(), allocated_bytes: 0, block_bytes_max: 0 };
    walk.account(root, &lstat(root, ".")?)?;
    let mut entries: Vec<(String, u64)> = Vec::new();
    let mut stack: Vec<PathBuf> = vec![PathBuf::new()];
    while let Some(relative) = stack.pop() {
        let listing =
            std::fs::read_dir(root.join(&relative)).map_err(|e| io_error(&relative, &e))?;
        for entry in listing {
            let entry = entry.map_err(|e| io_error(&relative, &e))?;
            if entries.len() == EMPTY_DIR_WALK_ENTRIES_MAX {
                return Err(WalkError::OverCap);
            }
            let child = relative.join(entry.file_name());
            let name = child.to_string_lossy().into_owned();
            let meta = lstat(&root.join(&child), &name)?;
            let bytes = walk.account(&child, &meta)?;
            if meta.file_type().is_dir() {
                stack.push(child);
            }
            entries.push((name, bytes));
        }
    }
    check_skeleton(&entries, cells)?;
    entries.sort();
    Ok(DirWalk {
        allocated_bytes: walk.allocated_bytes,
        block_bytes_max: walk.block_bytes_max,
        entries,
    })
}

struct Walker {
    seen: BTreeSet<(u64, u64)>,
    allocated_bytes: u64,
    block_bytes_max: u64,
}

impl Walker {
    /// Adds an inode's allocated bytes once; a second link adds 0.
    fn account(&mut self, path: &Path, meta: &std::fs::Metadata) -> Result<u64, WalkError> {
        self.block_bytes_max = self.block_bytes_max.max(meta.blksize());
        if !self.seen.insert((meta.dev(), meta.ino())) {
            return Ok(0);
        }
        let overflow = || WalkError::Io {
            path: path.display().to_string(),
            kind: std::io::ErrorKind::InvalidData,
        };
        let bytes = meta.blocks().checked_mul(STAT_BLOCK_BYTES).ok_or_else(overflow)?;
        self.allocated_bytes = self.allocated_bytes.checked_add(bytes).ok_or_else(overflow)?;
        Ok(bytes)
    }
}

fn lstat(path: &Path, name: &str) -> Result<std::fs::Metadata, WalkError> {
    std::fs::symlink_metadata(path).map_err(|e| io_error(Path::new(name), &e))
}

fn io_error(path: &Path, error: &std::io::Error) -> WalkError {
    WalkError::Io { path: path.display().to_string(), kind: error.kind() }
}

/// Every file the dir bill names: the four root files, and per cell its
/// `MANIFEST`, a log segment and a checkpoint. A walk that misses a
/// level cannot pass this.
fn check_skeleton(entries: &[(String, u64)], cells: u16) -> Result<(), WalkError> {
    let names: BTreeSet<&str> = entries.iter().map(|(name, _)| name.as_str()).collect();
    for file in NODE_ROOT_FILES {
        if !names.contains(file) {
            return Err(WalkError::NotANodeDir(file.to_string()));
        }
    }
    for cell in 0..cells {
        let manifest = format!("shard-{cell}/MANIFEST");
        if !names.contains(manifest.as_str()) {
            return Err(WalkError::NotANodeDir(manifest));
        }
        let segment = format!("shard-{cell}/log/seg-");
        if !names.iter().any(|n| n.starts_with(&segment) && n.ends_with(".ilog")) {
            return Err(WalkError::NotANodeDir(format!("{segment}*.ilog")));
        }
        let checkpoint = format!("shard-{cell}/ckpt/ckpt-");
        if !names.iter().any(|n| n.starts_with(&checkpoint) && n.ends_with(".ick")) {
            return Err(WalkError::NotANodeDir(format!("{checkpoint}*.ick")));
        }
    }
    Ok(())
}

/// The filler F = max(`DIR_PLANT_FILLER_MIN`, T + one step − W₀), the
/// subtraction saturating at 0, rounded up to a whole step. `None` only
/// when the rounding overflows (a threshold near `u64::MAX`).
pub(super) fn filler_bytes(threshold_bytes: u64, w0_bytes: u64) -> Option<u64> {
    threshold_bytes
        .saturating_add(DIR_STEP_BYTES)
        .saturating_sub(w0_bytes)
        .max(DIR_PLANT_FILLER_MIN)
        .checked_next_multiple_of(DIR_STEP_BYTES)
}

/// The dir row's planted red, on a stopped node directory that is never
/// booted again: W₀; F incompressible bytes written and fsynced at depth
/// 3 (`shard-0/log/plant-filler`); W₁ must rise by ≥ F and fail the loaded
/// row's comparator; a sparse `DIR_PLANT_SPARSE_BYTES` file must then
/// move the walk by under one step (W₂). All three walks see one state.
pub(super) fn dir_plant<W>(dir: &Path, gate: Option<&Gate>, mut walk: W) -> PlantVerdict
where
    W: FnMut(&Path) -> Result<u64, WalkError>,
{
    let Some(gate) = gate else {
        return PlantVerdict::Unengaged("the gates file has no empty data directory row".into());
    };
    let Some(threshold_bytes) = kib_to_bytes(gate.threshold) else {
        return PlantVerdict::Unengaged(format!(
            "threshold {} is not a byte count",
            gate.threshold
        ));
    };
    let log = dir.join("shard-0").join("log");
    let (filler, sparse) = (log.join("plant-filler"), log.join("plant-sparse"));
    let verdict = plant_walks(dir, (&filler, &sparse), gate, threshold_bytes, &mut walk);
    let removed =
        [&filler, &sparse].into_iter().filter(|path| std::fs::remove_file(path).is_ok()).count();
    match verdict {
        Ok(verdict) => verdict,
        Err(why) => PlantVerdict::Unengaged(format!("{why} ({removed} plant file(s) removed)")),
    }
}

fn plant_walks<W>(
    dir: &Path,
    (filler_path, sparse_path): (&Path, &Path),
    gate: &Gate,
    threshold_bytes: u64,
    walk: &mut W,
) -> Result<PlantVerdict, String>
where
    W: FnMut(&Path) -> Result<u64, WalkError>,
{
    let w0 = walk(dir).map_err(|e| format!("W0: {e}"))?;
    let filler = filler_bytes(threshold_bytes, w0).ok_or("the filler size overflows")?;
    write_filler(filler_path, filler)?;
    let w1 = walk(dir).map_err(|e| format!("W1: {e}"))?;
    std::fs::File::create(sparse_path)
        .and_then(|file| file.set_len(DIR_PLANT_SPARSE_BYTES))
        .map_err(|e| format!("sparse plant {}: {e}", sparse_path.display()))?;
    let w2 = walk(dir).map_err(|e| format!("W2: {e}"))?;
    Ok(judge_dir(gate, [w0, filler, w1, w2]))
}

/// Red only when the walk rose by the filler, the filled reading fails
/// the row's own comparator, and the sparse file moved it by under one
/// step (a walk that counts apparent size reads 1 GiB there).
pub(super) fn judge_dir(gate: &Gate, [w0, filler, w1, w2]: [u64; 4]) -> PlantVerdict {
    let detail = format!(
        "W0 {w0} B, F {filler} B, W1 {w1} B, W2 {w2} B; row {} {} {}",
        gate.comparator, gate.threshold, gate.unit
    );
    if w1.saturating_sub(w0) < filler {
        PlantVerdict::Green(format!("the walk did not see the filler: {detail}"))
    } else if gate.passes(bytes_to_kib(w1)) {
        PlantVerdict::Green(format!("W1 within the bill: {detail}"))
    } else if w2.abs_diff(w1) >= DIR_STEP_BYTES {
        PlantVerdict::Green(format!("the sparse file moved the walk: {detail}"))
    } else {
        PlantVerdict::Red(detail)
    }
}

pub(super) fn bytes_to_kib(bytes: u64) -> f64 {
    bytes as f64 / 1024.0
}

fn kib_to_bytes(kib: f64) -> Option<u64> {
    let bytes = (kib * 1024.0).ceil();
    // `u64::MAX as f64` rounds up to 2⁶⁴, so `<` keeps the cast exact.
    (bytes.is_finite() && bytes >= 0.0 && bytes < u64::MAX as f64).then_some(bytes as u64)
}

/// Writes `length_bytes` of splitmix64 output and fsyncs it, so the
/// blocks are allocated before the next walk.
fn write_filler(path: &Path, length_bytes: u64) -> Result<(), String> {
    const CHUNK_BYTES: usize = 4096;
    let fail = |e: std::io::Error| format!("filler {}: {e}", path.display());
    let mut file = std::fs::File::create(path).map_err(fail)?;
    let mut state = DIR_PLANT_FILLER_SEED;
    let mut chunk = [0u8; CHUNK_BYTES];
    let mut remaining = length_bytes;
    while remaining > 0 {
        for word in chunk.chunks_exact_mut(8) {
            word.copy_from_slice(&splitmix64(&mut state).to_le_bytes());
        }
        let take = usize::try_from(remaining).map_or(CHUNK_BYTES, |r| r.min(CHUNK_BYTES));
        file.write_all(&chunk[..take]).map_err(fail)?;
        remaining -= take as u64;
    }
    file.sync_all().map_err(fail)
}

fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}
