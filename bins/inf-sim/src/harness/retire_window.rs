//! O3's window (ADR-0156 D4): what the window's opening and close read from
//! the simulated disk alone, never from the cells' tables.
//!
//! `named` is every tier-file path a MANIFEST of the life named: each
//! shard's MANIFEST read at the life's recovery, then re-read at each
//! logged rename onto it. R0 is a file the disk still holds that a MANIFEST
//! of the life named and the shard's latest does not: a live namespace's
//! file a landed swap retired, which only the worker may remove. D is a
//! dropped namespace's tier files the disk still holds. A window opened at
//! a quiescent instant with `F = |R0 ∪ D|` files on `c` shards must end
//! them within `⌈F / 8⌉ + 5c + 1` steps: a lone position offers 8 a turn,
//! the worker runs 8 jobs a pass, and the walk's phases (a catalog, a
//! pass, the last results, the terminal step) cost each holding cell a few
//! more turns.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use inf_log::fs::sim::{DiskCall, SimDisk};

/// The files a window must end and the steps it has.
pub(crate) struct WindowBill {
    pub(crate) d: BTreeSet<PathBuf>,
    pub(crate) r0: BTreeSet<PathBuf>,
    /// `|R0 ∪ D|`.
    pub(crate) files: u64,
    /// The shards holding them.
    pub(crate) shards: u64,
    /// `⌈F / 8⌉ + 5c + 1`.
    pub(crate) steps: u64,
}

/// The MANIFESTs' names of one life.
pub(crate) struct RetireWindow {
    root: PathBuf,
    cells: u16,
    /// The disk log's next entry to read.
    next: usize,
    named: BTreeSet<PathBuf>,
    /// Each shard's latest MANIFEST's names.
    latest: BTreeMap<u16, BTreeSet<PathBuf>>,
}

/// The `shard-N` a path names.
pub(crate) fn shard_of(path: &Path) -> Option<u16> {
    path.components()
        .find_map(|part| part.as_os_str().to_str()?.strip_prefix("shard-")?.parse().ok())
}

/// The `ns-N` a path names.
pub(crate) fn ns_of(path: &Path) -> Option<u32> {
    path.components().find_map(|part| part.as_os_str().to_str()?.strip_prefix("ns-")?.parse().ok())
}

impl RetireWindow {
    pub(crate) fn new(root: PathBuf, cells: u16) -> RetireWindow {
        RetireWindow { root, cells, next: 0, named: BTreeSet::new(), latest: BTreeMap::new() }
    }

    /// A life recovered: its names begin with every shard's MANIFEST now.
    /// An unreadable MANIFEST is the breach returned.
    pub(crate) fn begin_life(&mut self, disk: &SimDisk) -> Vec<String> {
        self.named.clear();
        self.latest.clear();
        self.next = disk.logged_calls(0).len();
        (0..self.cells).filter_map(|shard| self.read_shard(disk, shard)).collect()
    }

    /// Reads the renames onto a `MANIFEST` logged since the last call, each
    /// shard's names re-read from the file the rename published.
    pub(crate) fn observe(&mut self, disk: &SimDisk) -> Vec<String> {
        let entries = disk.logged_calls(self.next);
        self.next += entries.len();
        let shards: BTreeSet<u16> = entries
            .iter()
            .filter(|entry| entry.call == DiskCall::ManifestRename)
            .filter_map(|entry| shard_of(&entry.path))
            .collect();
        shards.into_iter().filter_map(|shard| self.read_shard(disk, shard)).collect()
    }

    fn read_shard(&mut self, disk: &SimDisk, shard: u16) -> Option<String> {
        let dir = self.root.join(format!("shard-{shard}"));
        let manifest = match inf_log::manifest::read_manifest(disk, &dir) {
            Ok(manifest) => manifest,
            Err(err) => return Some(format!("RETIRE-R0: {} unreadable: {err}", dir.display())),
        };
        let mut names = BTreeSet::new();
        for section in manifest.iter().flat_map(|manifest| manifest.tiers.iter()) {
            let cold = dir.join(format!("ns-{}", section.ns)).join("cold");
            for file in &section.files {
                names.insert(cold.join(inf_log::tier_file_name(file.id)));
            }
        }
        self.named.extend(names.iter().cloned());
        self.latest.insert(shard, names);
        None
    }

    /// The namespaces a MANIFEST of the life named a tier file of.
    pub(crate) fn named_ns(&self) -> BTreeSet<u32> {
        self.named.iter().filter_map(|path| ns_of(path)).collect()
    }

    /// R0 now: each file on the disk a MANIFEST of the life named and its
    /// shard's latest does not, a dropped namespace's excepted.
    pub(crate) fn r0(
        &self,
        on_disk: &BTreeSet<PathBuf>,
        dropped: &BTreeSet<u32>,
    ) -> BTreeSet<PathBuf> {
        self.named
            .iter()
            .filter(|path| on_disk.contains(*path))
            .filter(|path| ns_of(path).is_none_or(|ns| !dropped.contains(&ns)))
            .filter(|path| {
                let shard = shard_of(path);
                !shard
                    .is_some_and(|shard| self.latest.get(&shard).is_some_and(|l| l.contains(*path)))
            })
            .cloned()
            .collect()
    }

    /// The bill of a window opened now: D the files of the namespaces in
    /// `life`, dropped in this life; R0 excepting every dropped namespace.
    pub(crate) fn bill(
        &self,
        on_disk: &BTreeSet<PathBuf>,
        life: &BTreeSet<u32>,
        dropped: &BTreeSet<u32>,
    ) -> WindowBill {
        let r0 = self.r0(on_disk, dropped);
        let d: BTreeSet<PathBuf> = on_disk
            .iter()
            .filter(|path| ns_of(path).is_some_and(|ns| life.contains(&ns)))
            .cloned()
            .collect();
        let both: BTreeSet<&PathBuf> = r0.iter().chain(d.iter()).collect();
        let shards: BTreeSet<u16> = both.iter().filter_map(|path| shard_of(path)).collect();
        let files = both.len() as u64;
        let shards = shards.len() as u64;
        WindowBill { d, r0, files, shards, steps: files.div_ceil(8) + 5 * shards + 1 }
    }
}
