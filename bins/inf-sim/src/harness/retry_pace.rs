//! O6, the retry pace (ADR-0156 D3): a retired tier file whose final close
//! or unlink failed is tried again no sooner than one injected millisecond
//! after the failure. The judge reads the simulated disk's own attempt log,
//! each close and unlink logged by the call that performs it, under the
//! harness's mark: an `unlink_tier` on a path after an entry on it whose
//! result is an injected `tier_close_fail` or `tier_unlink_fail` must lie at
//! least 1 ms of injected time after that entry. It holds on a correct
//! build because a result is consumed at a step after its failure and the
//! retry is due 1 ms after that consumption.
//!
//! Engagement, on a leg that arms a point: an injected failure of each
//! point the leg armed on a path a later `unlink_tier` removed, else
//! `VACUOUS`. Failures are matched within one boot: a reboot forgets them.

use std::collections::BTreeMap;
use std::path::PathBuf;

use inf_foundation::time::Nanos;
use inf_log::fs::sim::{CallResult, DiskCall, SimDisk};

/// The least injected time between a failed attempt and the next one on
/// its path (`TIER_RETIRE_RETRY_NS`).
const RETRY_GAP_NS: u64 = 1_000_000;

/// The points whose failures the clause anchors on.
const POINTS: [&str; 2] = [inf_log::fault::TIER_CLOSE_FAIL, inf_log::fault::TIER_UNLINK_FAIL];

#[derive(Default)]
pub(crate) struct RetryPace {
    /// The disk log's next entry to read.
    next: usize,
    /// Each path's injected failures since its last removal: the points
    /// that failed on it and the last failure's time.
    failed: BTreeMap<PathBuf, (Vec<&'static str>, Nanos)>,
    /// The points the leg armed.
    armed: Vec<&'static str>,
    /// The points that failed on a path a later unlink removed.
    engaged: Vec<&'static str>,
    /// Retries judged: an attempt after a failure on its path.
    pub(crate) retries: u64,
}

impl RetryPace {
    /// The leg armed `point` (one of the clause's two).
    pub(crate) fn armed(&mut self, point: &'static str) {
        if !self.armed.contains(&point) {
            self.armed.push(point);
        }
    }

    /// A reboot: failures of the life before are not matched across it.
    pub(crate) fn reboot(&mut self, disk: &SimDisk) {
        self.next = disk.logged_calls(0).len();
        self.failed.clear();
    }

    /// Reads the entries logged since the last call; returns each breach.
    pub(crate) fn judge(&mut self, disk: &SimDisk) -> Vec<String> {
        let entries = disk.logged_calls(self.next);
        self.next += entries.len();
        let mut breaches = Vec::new();
        for entry in entries {
            let at = entry.mark.map_or(Nanos(0), |mark| mark.at);
            let injected = match entry.result {
                CallResult::Injected(point) if POINTS.contains(&point) => Some(point),
                CallResult::Injected(_) | CallResult::Ok | CallResult::Failed(_) => None,
            };
            match entry.call {
                DiskCall::FinalClose => {}
                DiskCall::UnlinkTier => {
                    if let Some((points, failed_at)) = self.failed.get(&entry.path) {
                        self.retries += 1;
                        if at.0 < failed_at.0.saturating_add(RETRY_GAP_NS) {
                            breaches.push(format!(
                                "RETRY-PACE: {} unlinked again at {at:?}, within 1 ms of its \
                                 {} at {failed_at:?}",
                                entry.path.display(),
                                points.last().copied().unwrap_or("failure")
                            ));
                        }
                        if entry.result == CallResult::Ok {
                            for point in points {
                                if !self.engaged.contains(point) {
                                    self.engaged.push(point);
                                }
                            }
                        }
                    }
                }
                DiskCall::CheckpointCreate { .. }
                | DiskCall::ManifestRename
                | DiskCall::CreateTier { .. }
                | DiskCall::RemoveTier => continue,
            }
            match injected {
                Some(point) => {
                    let failed = self.failed.entry(entry.path).or_default();
                    if !failed.0.contains(&point) {
                        failed.0.push(point);
                    }
                    failed.1 = at;
                }
                None if entry.call == DiskCall::UnlinkTier => {
                    self.failed.remove(&entry.path);
                }
                None => {}
            }
        }
        breaches
    }

    /// Each armed point's engagement, else `VACUOUS`.
    pub(crate) fn engagement(&self) -> Vec<String> {
        self.armed
            .iter()
            .filter(|point| !self.engaged.contains(point))
            .map(|point| {
                format!(
                    "VACUOUS RETRY-PACE: no {point} failure on a path a later unlink removed \
                     ({} retries judged)",
                    self.retries
                )
            })
            .collect()
    }
}
