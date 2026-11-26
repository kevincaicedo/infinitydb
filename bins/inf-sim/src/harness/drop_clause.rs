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

use std::path::Path;

use inf_foundation::time::Nanos;
use inf_log::fs::sim::{DiskCall, DiskPhase, SimDisk};

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
