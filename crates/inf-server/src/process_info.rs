//! Process-wide INFO gauges, sampled by the node's control owner (ADR-0144 D5).
//! Cells hold only the read board. Before sampling, gauges are zero; a failed
//! read retains the last successful value. Each gauge is independent, so a
//! reader need not spin for a coherent cross-gauge snapshot.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(any(target_os = "linux", test))]
mod stat;

/// Last observed process gauges. CPU values are microseconds, RSS is bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ProcessSample {
    pub rss_bytes: u64,
    pub cpu_sys_us: u64,
    pub cpu_user_us: u64,
}

/// Read-only to cells; only [`ProcessSampler`] can publish measurements.
#[derive(Default, Debug)]
pub struct ProcessBoard {
    rss_bytes: AtomicU64,
    cpu_sys_us: AtomicU64,
    cpu_user_us: AtomicU64,
    /// Control-owner polls, including polls whose OS reads failed.
    polls: AtomicU64,
}

impl ProcessBoard {
    pub fn read(&self) -> ProcessSample {
        ProcessSample {
            rss_bytes: self.rss_bytes.load(Ordering::Relaxed),
            cpu_sys_us: self.cpu_sys_us.load(Ordering::Relaxed),
            cpu_user_us: self.cpu_user_us.load(Ordering::Relaxed),
        }
    }

    pub fn polls(&self) -> u64 {
        self.polls.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(crate) fn fixture(sample: ProcessSample) -> Arc<Self> {
        let mut sampler = ProcessSampler::default();
        sampler.publish(Some(sample.rss_bytes), Some((sample.cpu_sys_us, sample.cpu_user_us)));
        sampler.board()
    }
}

/// Sole writer, held by the process supervisor, never by a cell or connection.
#[derive(Default)]
pub struct ProcessSampler {
    board: Arc<ProcessBoard>,
}

impl ProcessSampler {
    pub fn board(&self) -> Arc<ProcessBoard> {
        Arc::clone(&self.board)
    }

    /// Read the OS outside every cell. The supervisor bounds the cadence.
    pub fn sample(&mut self) {
        self.publish(inf_runtime::net::process_rss_bytes(), read_cpu());
    }

    fn publish(&mut self, rss: Option<u64>, cpu: Option<(u64, u64)>) {
        if let Some(rss) = rss {
            self.board.rss_bytes.store(rss, Ordering::Relaxed);
        }
        if let Some((sys, user)) = cpu {
            self.board.cpu_sys_us.store(sys, Ordering::Relaxed);
            self.board.cpu_user_us.store(user, Ordering::Relaxed);
        }
        let polls = self.board.polls.load(Ordering::Relaxed).saturating_add(1);
        self.board.polls.store(polls, Ordering::Relaxed);
    }
}

#[cfg(target_os = "linux")]
#[allow(clippy::disallowed_types, reason = "control-thread: bounded CPU sample")]
fn read_cpu() -> Option<(u64, u64)> {
    use std::io::Read;

    let mut text = String::new();
    std::fs::File::open("/proc/self/stat")
        .ok()?
        .take(crate::limits::PROCESS_STAT_BYTES_MAX + 1)
        .read_to_string(&mut text)
        .ok()?;
    stat::cpu_microseconds(&text)
}

#[cfg(not(target_os = "linux"))]
fn read_cpu() -> Option<(u64, u64)> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_is_zero_and_failed_reads_retain_the_last_success() {
        let mut sampler = ProcessSampler::default();
        let board = sampler.board();
        assert_eq!(board.read(), ProcessSample::default());
        assert_eq!(board.polls(), 0);
        sampler.publish(None, None);
        assert_eq!(board.read(), ProcessSample::default());
        sampler.publish(Some(4096), Some((120_000, 340_000)));
        let expected = ProcessSample { rss_bytes: 4096, cpu_sys_us: 120_000, cpu_user_us: 340_000 };
        assert_eq!(board.read(), expected);
        sampler.publish(None, None);
        assert_eq!(board.read(), expected);
        assert_eq!(board.polls(), 3);
    }
}
