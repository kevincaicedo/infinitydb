//! On-demand, cell-owned reactor snapshots for `INFO loophist` (ADR-0136).

use core::cell::{Cell, RefCell};
use core::fmt::Write;

use inf_foundation::LogHistogram;

#[derive(Default, Debug)]
struct Snapshot {
    histogram: LogHistogram,
    counters: [u64; 6],
}

/// A request is completed after the reactor iteration that received it.
/// Readers wait for a newer sample count on their next INFO round trip.
#[derive(Default, Debug)]
pub struct LoopSnapshot {
    requested: Cell<bool>,
    completed: RefCell<Option<Snapshot>>,
}

impl LoopSnapshot {
    /// Allocated bucket storage, separate from the data maxmemory budget.
    pub fn reserved_bytes(&self) -> u64 {
        if self.completed.borrow().is_some() {
            (LogHistogram::BUCKET_COUNT * core::mem::size_of::<u64>()) as u64
        } else {
            0
        }
    }

    /// Called by the owner after an iteration; copies only on a scrape request.
    pub fn capture_if_requested(&self, histogram: &LogHistogram, counters: [u64; 6]) {
        if !self.requested.replace(false) {
            return;
        }
        let mut completed = self.completed.borrow_mut();
        let snapshot = completed.get_or_insert_with(Snapshot::default);
        snapshot.histogram.copy_from(histogram);
        snapshot.counters = counters;
    }

    pub(crate) fn append_info(&self, text: &mut String) {
        self.requested.set(true);
        text.push_str("# Loop histogram\r\nloop_histogram_schema:1\r\n");
        let completed = self.completed.borrow();
        let Some(snapshot) = completed.as_ref() else {
            text.push_str("loop_histogram_pending:1\r\n");
            return;
        };
        let _ = write!(text, "loop_histogram_samples:{}\r\n", snapshot.histogram.count());
        let _ = write!(text, "loop_histogram_iterations:{}\r\n", snapshot.counters[3]);
        let _ = write!(text, "loop_histogram_submits:{}\r\n", snapshot.counters[0]);
        let _ = write!(text, "loop_histogram_sqes:{}\r\n", snapshot.counters[1]);
        text.push_str("loop_histogram_counts:");
        for (index, count) in snapshot.histogram.bucket_counts().iter().enumerate() {
            if index != 0 {
                text.push(',');
            }
            let _ = write!(text, "{count}");
        }
        text.push_str("\r\n\r\n");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshots_are_requested_coherent_and_independent_of_later_iterations() {
        let snapshot = LoopSnapshot::default();
        assert_eq!(snapshot.reserved_bytes(), 0);
        let mut histogram = LogHistogram::new();
        histogram.record(0);
        snapshot.capture_if_requested(&histogram, [1, 16, 0, 1, 0, 0]);
        assert!(snapshot.completed.borrow().is_none());
        let mut info = String::new();
        snapshot.append_info(&mut info);
        assert!(info.contains("loop_histogram_pending:1"));
        snapshot.capture_if_requested(&histogram, [1, 16, 0, 1, 0, 0]);
        histogram.record(1000);
        assert_eq!(snapshot.reserved_bytes(), 15360);
        snapshot.capture_if_requested(&histogram, [2, 32, 0, 2, 0, 0]);
        info.clear();
        snapshot.append_info(&mut info);
        assert!(info.contains("loop_histogram_samples:1\r\n"));
        assert!(info.contains("loop_histogram_submits:1\r\n"));
        snapshot.capture_if_requested(&histogram, [2, 32, 0, 2, 0, 0]);
        info.clear();
        snapshot.append_info(&mut info);
        assert!(info.contains("loop_histogram_samples:2\r\n"));
        assert!(info.contains("loop_histogram_sqes:32\r\n"));
    }
}
