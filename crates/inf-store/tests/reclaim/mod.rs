//! The reclaim oracle of the real-filesystem legs (ADR-0200 D7): after a
//! leg's last unlink, this process holds no file the leg unlinked under its
//! root. Every half reads what this process holds, never a counter other
//! processes change, so a sibling test or another run on the same volume
//! cannot turn it red, and a leaked file cannot hide under its slack.
//!
//! - **Path.** A walk of the whole root (every regular file, links not
//!   followed) finds, in summed `st_blocks`, exactly the blocks of the
//!   residual the leg names: the tier files its flush still owns.
//! - **Descriptor.** No entry of `/proc/self/fd` names a path under the root
//!   marked ` (deleted)`.
//! - **Mapping.** No line of `/proc/self/maps` does.
//!
//! The descriptor and mapping halves compile on Linux only, where `/proc`
//! shows them; there the legs are authoritative for all three, and elsewhere
//! the path half runs alone. The legs write through std's files and the
//! store maps no file (its mappings are anonymous), so a file the leg
//! unlinked that is still held is under the root, in a descriptor or in a
//! mapping; one moved outside the root and closed is not seen.
//! `reclaim_oracle.rs` plants each half red.

use std::path::{Path, PathBuf};

/// What this process still holds under a leg's root, beside the residual.
#[derive(Debug, Default)]
pub struct Holdings {
    /// Summed `st_blocks` of every regular file under the root.
    pub blocks_found: u64,
    /// Summed `st_blocks` of the residual the leg named.
    pub blocks_residual: u64,
    /// Regular files under the root, outside the residual, with their blocks.
    pub strays: Vec<(PathBuf, u64)>,
    /// Residual paths that are no regular file under the root.
    pub residual_absent: Vec<PathBuf>,
    /// Descriptors of this process on a file unlinked under the root.
    pub descriptors: Vec<String>,
    /// Mappings of this process of a file unlinked under the root.
    pub mappings: Vec<String>,
}

impl Holdings {
    /// The leg reclaimed everything it unlinked: the walk's blocks are the
    /// residual's, every residual file was found, and nothing unlinked is held.
    pub fn is_clean(&self) -> bool {
        self.blocks_found == self.blocks_residual
            && self.residual_absent.is_empty()
            && self.descriptors.is_empty()
            && self.mappings.is_empty()
    }
}

/// Reads all three halves for `root`, whose regular files may hold only the
/// `residual` (absolute or relative to the working directory; each must be a
/// regular file under the root).
pub fn holdings(root: &Path, residual: &[PathBuf]) -> Holdings {
    let root = std::fs::canonicalize(root)
        .unwrap_or_else(|e| panic!("the leg's root {} does not resolve: {e}", root.display()));
    let mut wanted: Vec<PathBuf> = Vec::with_capacity(residual.len());
    let mut report = Holdings::default();
    for path in residual {
        match std::fs::canonicalize(path) {
            Ok(path) if path.starts_with(&root) => wanted.push(path),
            Ok(_) | Err(_) => report.residual_absent.push(path.clone()),
        }
    }
    for (path, blocks) in regular_files(&root) {
        report.blocks_found += blocks;
        if let Some(index) = wanted.iter().position(|w| *w == path) {
            wanted.swap_remove(index);
            report.blocks_residual += blocks;
        } else {
            report.strays.push((path, blocks));
        }
    }
    report.residual_absent.extend(wanted);
    #[cfg(target_os = "linux")]
    {
        report.descriptors = deleted_descriptors(&root);
        report.mappings = deleted_mappings(&root);
    }
    report
}

/// The leg's assertion: [`holdings`] is clean, or the panic names every
/// stray, absent residual, descriptor and mapping.
pub fn assert_reclaimed(root: &Path, residual: &[PathBuf]) {
    let report = holdings(root, residual);
    assert!(
        report.is_clean(),
        "the leg left space held under {}: {} blocks found against the residual's {} \
         (strays {:?}, residual absent {:?}); unlinked files still held by descriptor {:?}, \
         by mapping {:?}",
        root.display(),
        report.blocks_found,
        report.blocks_residual,
        report.strays,
        report.residual_absent,
        report.descriptors,
        report.mappings,
    );
}

/// Every regular file under `root` with its `st_blocks`, walked with an
/// explicit stack: a symbolic link is neither followed nor counted.
fn regular_files(root: &Path) -> Vec<(PathBuf, u64)> {
    use std::os::unix::fs::MetadataExt;

    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let entries = std::fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("the walk cannot list {}: {e}", dir.display()));
        for entry in entries {
            let entry = entry.unwrap_or_else(|e| panic!("the walk of {}: {e}", dir.display()));
            let kind = entry.file_type().expect("an entry's type");
            if kind.is_dir() {
                pending.push(entry.path());
            } else if kind.is_file() {
                let meta = std::fs::symlink_metadata(entry.path()).expect("an entry's metadata");
                found.push((entry.path(), meta.blocks()));
            }
        }
    }
    found
}

/// The link text of a path under `root` the kernel marks deleted.
#[cfg(target_os = "linux")]
fn names_deleted_under(text: &str, root: &Path) -> bool {
    text.strip_suffix(" (deleted)").is_some_and(|path| Path::new(path).starts_with(root))
}

/// This process's descriptors on a file unlinked under `root`. A descriptor
/// another thread closes between the listing and its link is skipped: it
/// was not this leg's, whose thread is the one reading.
#[cfg(target_os = "linux")]
fn deleted_descriptors(root: &Path) -> Vec<String> {
    let mut held = Vec::new();
    let mut listed = 0usize;
    let entries = std::fs::read_dir("/proc/self/fd").expect("/proc/self/fd lists");
    for entry in entries {
        let entry = entry.expect("a /proc/self/fd entry");
        listed += 1;
        match std::fs::read_link(entry.path()) {
            Ok(target) => {
                let text = target.to_string_lossy().into_owned();
                if names_deleted_under(&text, root) {
                    held.push(format!("fd {}: {text}", entry.file_name().to_string_lossy()));
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => panic!("/proc/self/fd/{}: {e}", entry.file_name().to_string_lossy()),
        }
    }
    // The listing's own descriptor is open while it runs, so an empty one
    // read nothing: the descriptor half would see no held file.
    assert!(listed > 0, "/proc/self/fd listed no descriptor");
    held
}

/// This process's mappings of a file unlinked under `root`: the path field
/// of `/proc/self/maps` follows five space-separated fields.
#[cfg(target_os = "linux")]
fn deleted_mappings(root: &Path) -> Vec<String> {
    let maps = std::fs::read_to_string("/proc/self/maps").expect("/proc/self/maps reads");
    assert!(!maps.is_empty(), "/proc/self/maps is empty: the mapping half would see nothing");
    maps.lines()
        .filter(|line| {
            let path = line.splitn(6, ' ').nth(5).unwrap_or("").trim_start();
            names_deleted_under(path, root)
        })
        .map(str::to_owned)
        .collect()
}
