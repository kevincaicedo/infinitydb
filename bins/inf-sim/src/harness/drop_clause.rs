//! The drop clause (ADR-0156 D3): once a `DROP` of a durable tiered
//! namespace has answered `+OK`, every unlink of that namespace's tier
//! files is the control thread's retirement worker's, never a cell's. The
//! judge reads the simulated disk's own unlink entries, each logged by the
//! `unlink_tier` call that performs it under the harness's mark (a cell's
//! iteration or the worker's pass), never anything the code under test
//! reports.
//!
//! The bound is the step whose reply the harness read, not the boot: the
//! origin replies only after every cell applied the drop, so from that
//! step on every cell holds the namespace Draining and no cell-side path
//! names its files; before it a peer still Live may unlink a file its live
//! life retired, on the cell.
//!
//! O3's per-step clause (ADR-0156 D3, D4), opted into by a leg that judges
//! once a step: once every cell has recovered, every tier-file unlink the
//! disk logs (`unlink_tier` and `remove_file` on a `tier-NNNNNN.itier`
//! name alike) carries the worker's mark, and a step makes at most
//! `RETIRE_WORKER_JOBS_PER_PASS` of them. Recovery's own sweeps run in the
//! cells' iterations before that and are not judged; a reboot pauses the
//! clause until the new life has recovered.

use std::path::Path;

use inf_foundation::time::Nanos;
use inf_log::fs::sim::{DiskCall, DiskPhase, SimDisk};

/// Tier-file unlinks one worker pass makes at most (the worker's jobs a
/// pass, `inf_server::limits::RETIRE_WORKER_JOBS_PER_PASS`): the simulator
/// runs one pass a step, so a step makes at most this many.
const UNLINKS_PER_STEP_MAX: u64 = 8;

/// The drops whose `+OK` the harness read, and what the judge has read.
#[derive(Default)]
pub(crate) struct DropClause {
    /// `(namespace id, injected time the reply was read at)`.
    drops: Vec<(u32, Nanos)>,
    /// The disk log's next entry to read.
    next: usize,
    /// Worker-marked unlinks of a dropped namespace's tier file after its
    /// reply: the clause's engagement.
    engaged: u64,
    /// O3's per-step clause: judged while `Some`, once every cell of the
    /// life has recovered.
    per_step: Option<PerStep>,
}

/// O3's per-step clause's state.
#[derive(Default)]
struct PerStep {
    /// The life has recovered: its unlinks are judged.
    judging: bool,
    /// The most tier-file unlinks one judged step made: the engagement
    /// reads a pass that made its whole allowance.
    unlinks_step_max: u64,
}

/// A tier file's namespace id, from its path's `ns-<id>` component, when
/// its name is a tier file's (`tier-NNNNNN.itier`).
fn tier_file_ns(path: &Path) -> Option<u32> {
    let file = path.file_name()?.to_str()?;
    if !(file.starts_with("tier-") && file.ends_with(".itier")) {
        return None;
    }
    path.components().find_map(|part| part.as_os_str().to_str()?.strip_prefix("ns-")?.parse().ok())
}

impl DropClause {
    /// A drop clause that also judges O3's per-step clause; the caller
    /// judges once a step and calls [`recovered`](Self::recovered) once a
    /// life has recovered.
    pub(crate) fn per_step() -> DropClause {
        DropClause { per_step: Some(PerStep::default()), ..DropClause::default() }
    }

    /// Every cell of the life has recovered: from the disk's next entry on,
    /// its tier-file unlinks are judged.
    pub(crate) fn recovered(&mut self, disk: &SimDisk) {
        self.next = disk.logged_calls(0).len();
        if let Some(step) = self.per_step.as_mut() {
            step.judging = true;
        }
    }

    /// A reboot: the new life's recovery sweeps are not judged until
    /// [`recovered`](Self::recovered) is called again.
    pub(crate) fn reboot(&mut self) {
        if let Some(step) = self.per_step.as_mut() {
            step.judging = false;
        }
    }

    /// The ids of the namespaces whose `DROP` the harness read.
    pub(crate) fn dropped(&self) -> impl Iterator<Item = u32> + '_ {
        self.drops.iter().map(|&(id, _)| id)
    }

    /// The most tier-file unlinks one judged step made.
    pub(crate) fn unlinks_step_max(&self) -> u64 {
        self.per_step.as_ref().map_or(0, |step| step.unlinks_step_max)
    }

    /// O3's per-step clause over one step's entries.
    fn judge_step(&mut self, entries: &[inf_log::fs::sim::LoggedCall], breaches: &mut Vec<String>) {
        let Some(step) = self.per_step.as_mut() else { return };
        if !step.judging {
            return;
        }
        let mut unlinks = 0u64;
        let tier_unlinks = entries.iter().filter(|entry| {
            matches!(entry.call, DiskCall::UnlinkTier | DiskCall::RemoveTier)
                && tier_file_ns(&entry.path).is_some()
        });
        for entry in tier_unlinks {
            unlinks += 1;
            match entry.mark.map(|mark| mark.phase) {
                Some(DiskPhase::Worker) => {}
                phase @ (Some(DiskPhase::Cell(_)) | None) => breaches.push(format!(
                    "RETIRE-STEP: {} unlinked under {phase:?}, not the worker's pass, after \
                     recovery ({:?})",
                    entry.path.display(),
                    entry.call
                )),
            }
        }
        step.unlinks_step_max = step.unlinks_step_max.max(unlinks);
        if unlinks > UNLINKS_PER_STEP_MAX {
            breaches.push(format!(
                "RETIRE-STEP: {unlinks} tier-file unlinks in one step, above the pass's \
                 {UNLINKS_PER_STEP_MAX}"
            ));
        }
    }

    /// The harness read `+OK` for the `DROP` of namespace `ns` at `at`.
    pub(crate) fn note_drop(&mut self, ns: u32, at: Nanos) {
        self.drops.push((ns, at));
    }

    /// Reads the unlink entries logged since the last call: each on a tier
    /// file of a namespace dropped before its mark must carry the worker's
    /// mark. Returns each breach, and a log that overflowed.
    pub(crate) fn judge(&mut self, disk: &SimDisk) -> Vec<String> {
        let entries = disk.logged_calls(self.next);
        self.next += entries.len();
        let mut breaches = Vec::new();
        if disk.calls_unlogged() > 0 {
            breaches.push(format!(
                "DROP-CLAUSE: the disk's call log overflowed ({} calls unlogged): the clause \
                 cannot read every unlink",
                disk.calls_unlogged()
            ));
        }
        self.judge_step(&entries, &mut breaches);
        for entry in entries.iter().filter(|entry| entry.call == DiskCall::UnlinkTier) {
            let Some(ns) = tier_file_ns(&entry.path) else { continue };
            let Some(mark) = entry.mark else { continue };
            let dropped = self.drops.iter().any(|&(id, at)| id == ns && mark.at >= at);
            if !dropped {
                continue;
            }
            match mark.phase {
                DiskPhase::Worker => self.engaged += 1,
                DiskPhase::Cell(cell) => breaches.push(format!(
                    "DROP-CLAUSE: cell {} unlinked {} of dropped namespace {ns} at {:?}, after \
                     its DROP answered",
                    cell.0,
                    entry.path.display(),
                    mark.at
                )),
            }
        }
        breaches
    }

    /// O3's per-step engagement: a judged step in which the worker's pass
    /// made its whole allowance of tier-file unlinks, else `VACUOUS`.
    pub(crate) fn step_engagement(&self) -> Option<String> {
        let max = self.unlinks_step_max();
        if max >= UNLINKS_PER_STEP_MAX {
            return None;
        }
        Some(format!(
            "VACUOUS RETIRE-STEP: no judged step made {UNLINKS_PER_STEP_MAX} tier-file unlinks \
             (the most: {max})"
        ))
    }

    /// The clause's engagement: a worker's unlink of a dropped namespace's
    /// tier file after its reply was read, else `VACUOUS`.
    pub(crate) fn engagement(&self) -> Option<String> {
        if self.engaged > 0 {
            return None;
        }
        Some(format!(
            "VACUOUS drop clause: no worker unlinked a dropped namespace's tier file after its \
             DROP answered ({} drops read)",
            self.drops.len()
        ))
    }
}
