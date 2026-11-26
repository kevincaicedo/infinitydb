//! O7, failed creates (ADR-0156 D1): a tier file's create that fails at or
//! after `create_tier` leaves its name owned by the pipeline's residue slot
//! until the control thread's unlink of it answers, and no create meets the
//! residue's file or its unlink in flight. The judge reads the simulated
//! disk's own create and unlink logs, each entry written by the call that
//! performs it (both create points fire inside `create_tier`), never what
//! the pipeline reports:
//!
//! - no `create_tier` answers `EEXIST`;
//! - between a failed `create_tier` on a path and the next one on it lies
//!   an `unlink_tier` of it in a worker pass answering `Ok` or `NotFound`;
//! - a namespace's next `create_tier` after a failed one names the same
//!   path (no file id spent);
//! - at the leg's end no file a failed create left remains.
//!
//! Engagement: each create point failed a create whose residue's unlink
//! the harness held across a MAINTAIN, and that path was created after it,
//! else `VACUOUS`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use inf_log::fs::sim::{CallResult, DiskCall, DiskPhase, SimDisk};

/// The two create points.
const POINTS: [&str; 2] = [inf_log::fault::TIER_CREATE_FAIL, inf_log::fault::TIER_CREATE_POST_FAIL];

/// One path's create history.
#[derive(Default)]
struct PathState {
    /// Its last create failed and no terminal unlink of it followed.
    residue: bool,
    /// Its last create failed (whatever followed it).
    last_failed: bool,
    /// The points that failed a create of it, not yet followed by one that
    /// made it.
    failed_points: Vec<&'static str>,
}

#[derive(Default)]
pub(crate) struct CreateClause {
    next: usize,
    paths: BTreeMap<PathBuf, PathState>,
    /// Each namespace directory's last failed create's path, until a create
    /// in it succeeds.
    dirs: BTreeMap<PathBuf, PathBuf>,
    /// The points whose failed create's path was made again afterwards.
    created_after: Vec<&'static str>,
    /// The points whose residue's unlink the harness held across a MAINTAIN.
    held: Vec<&'static str>,
    /// Failed creates the clause read.
    pub(crate) failed_creates: u64,
    /// Injected unlink failures of a residue's file: a `Retry` the slot
    /// must keep.
    pub(crate) residue_unlink_retries: u64,
}

impl CreateClause {
    /// The harness held `point`'s residue's unlink across a MAINTAIN.
    pub(crate) fn held(&mut self, point: &'static str) {
        if !self.held.contains(&point) {
            self.held.push(point);
        }
    }

    /// Every cell of the life has recovered: judged from the next entry.
    pub(crate) fn recovered(&mut self, disk: &SimDisk) {
        self.next = disk.logged_calls(0).len();
    }

    /// Reads the entries logged since the last call; returns each breach.
    pub(crate) fn judge(&mut self, disk: &SimDisk) -> Vec<String> {
        let entries = disk.logged_calls(self.next);
        self.next += entries.len();
        let mut breaches = Vec::new();
        for entry in entries {
            match entry.call {
                DiskCall::CreateTier { .. } => {
                    self.create(&entry.path, entry.result, &mut breaches)
                }
                DiskCall::UnlinkTier => {
                    let terminal = matches!(
                        entry.result,
                        CallResult::Ok | CallResult::Failed(std::io::ErrorKind::NotFound)
                    );
                    let worker = entry.mark.is_some_and(|mark| mark.phase == DiskPhase::Worker);
                    let failed =
                        entry.result == CallResult::Injected(inf_log::fault::TIER_UNLINK_FAIL);
                    if let Some(state) = self.paths.get_mut(&entry.path) {
                        if terminal && worker {
                            state.residue = false;
                        }
                        if failed && state.residue {
                            self.residue_unlink_retries += 1;
                        }
                    }
                }
                DiskCall::CheckpointCreate { .. }
                | DiskCall::ManifestRename
                | DiskCall::FinalClose
                | DiskCall::RemoveTier => {}
            }
        }
        breaches
    }

    fn create(&mut self, path: &Path, result: CallResult, breaches: &mut Vec<String>) {
        if result == CallResult::Failed(std::io::ErrorKind::AlreadyExists) {
            breaches
                .push(format!("FAILED-CREATE: create_tier of {} answered EEXIST", path.display()));
        }
        let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
        if let Some(failed) = self.dirs.get(&dir)
            && failed != path
        {
            breaches.push(format!(
                "FAILED-CREATE: the create after the failed one of {} names {}, a fresh path",
                failed.display(),
                path.display()
            ));
        }
        let state = self.paths.entry(path.to_path_buf()).or_default();
        if state.residue {
            breaches.push(format!(
                "FAILED-CREATE: {} created again with no worker unlink of its residue since its \
                 failed create",
                path.display()
            ));
        }
        match result {
            CallResult::Ok => {
                for point in state.failed_points.drain(..) {
                    if !self.created_after.contains(&point) {
                        self.created_after.push(point);
                    }
                }
                state.residue = false;
                state.last_failed = false;
                self.dirs.remove(&dir);
            }
            CallResult::Injected(_) | CallResult::Failed(_) => {
                self.failed_creates += 1;
                if let CallResult::Injected(point) = result
                    && POINTS.contains(&point)
                    && !state.failed_points.contains(&point)
                {
                    state.failed_points.push(point);
                }
                state.residue = true;
                state.last_failed = true;
                self.dirs.insert(dir, path.to_path_buf());
            }
        }
    }

    /// The end clause: no file a failed create left is still named.
    pub(crate) fn left_files(&self, disk: &SimDisk) -> Vec<String> {
        let named = disk.names_where(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("tier-") && name.ends_with(".itier"))
        });
        self.paths
            .iter()
            .filter(|(path, state)| state.last_failed && named.contains(path))
            .map(|(path, _)| {
                format!("FAILED-CREATE: {} a failed create left is on the disk", path.display())
            })
            .collect()
    }

    /// Each create point's engagement, else `VACUOUS`.
    pub(crate) fn engagement(&self) -> Vec<String> {
        POINTS
            .iter()
            .filter(|point| !(self.held.contains(point) && self.created_after.contains(point)))
            .map(|point| {
                format!(
                    "VACUOUS FAILED-CREATE: {point} failed no create whose residue's unlink was \
                     held and whose path was made after it ({} failed creates read)",
                    self.failed_creates
                )
            })
            .collect()
    }
}
