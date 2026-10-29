//! ADR-0146: all cell caches exist before any plane submits its first accept.
//! Only startup readiness crosses threads; caches never leave their cell.

use core::num::NonZeroU16;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};

#[derive(Debug)]
pub(crate) struct BootBoard {
    pending_cells: AtomicU16,
    failed: AtomicBool,
}

/// Exactly one construction permit per cell in the validated boot topology.
/// Dropping this iterator early fails the group instead of stranding its peers.
#[derive(Debug)]
pub struct CacheBootGroup {
    board: Arc<BootBoard>,
    unissued: u16,
}

impl CacheBootGroup {
    /// No per-cell backing allocation: issuing each permit is constant work.
    pub fn new(cells: NonZeroU16) -> Self {
        Self {
            board: Arc::new(BootBoard {
                pending_cells: AtomicU16::new(cells.get()),
                failed: AtomicBool::new(false),
            }),
            unissued: cells.get(),
        }
    }
}

impl Iterator for CacheBootGroup {
    type Item = CacheBootPermit;

    fn next(&mut self) -> Option<Self::Item> {
        self.unissued = self.unissued.checked_sub(1)?;
        Some(CacheBootPermit { board: Arc::clone(&self.board), arrived: false })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = usize::from(self.unissued);
        (remaining, Some(remaining))
    }
}

impl ExactSizeIterator for CacheBootGroup {}
impl core::iter::FusedIterator for CacheBootGroup {}

impl Drop for CacheBootGroup {
    fn drop(&mut self) {
        if self.unissued != 0 {
            self.board.failed.store(true, Ordering::Release);
        }
    }
}

/// A linear arrival. Only a successfully built NodeInfo can complete it.
/// Failure, unwind, or abandoning a cell leaves every peer's listener unarmed.
#[derive(Debug)]
#[must_use = "an unused cache boot permit fails the whole boot group"]
pub struct CacheBootPermit {
    board: Arc<BootBoard>,
    arrived: bool,
}

impl CacheBootPermit {
    pub(crate) fn reader(&self) -> CacheBootReadiness {
        CacheBootReadiness::Group(Arc::clone(&self.board))
    }

    pub(crate) fn complete(mut self) {
        self.arrived = true;
        // The iterator issues exactly the initial count, and a permit is
        // consumed once. Refusal leaves a pending participant, never wraps.
        self.board.pending_cells.fetch_sub(1, Ordering::Release);
    }
}

impl Drop for CacheBootPermit {
    fn drop(&mut self) {
        if !self.arrived {
            self.board.failed.store(true, Ordering::Release);
        }
    }
}

#[derive(Debug)]
pub(crate) enum CacheBootReadiness {
    Standalone,
    Group(Arc<BootBoard>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CacheBootStatus {
    Pending,
    Ready,
    Failed,
}

impl CacheBootReadiness {
    pub(crate) fn status(&self) -> CacheBootStatus {
        match self {
            Self::Standalone => CacheBootStatus::Ready,
            Self::Group(board) => {
                if board.pending_cells.load(Ordering::Acquire) == 0 {
                    // Zero means every permit completed; failure and new
                    // participants are impossible after this observation.
                    CacheBootStatus::Ready
                } else if board.failed.load(Ordering::Acquire) {
                    CacheBootStatus::Failed
                } else {
                    CacheBootStatus::Pending
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group(cells: u16) -> CacheBootGroup {
        CacheBootGroup::new(NonZeroU16::new(cells).unwrap())
    }

    #[test]
    fn each_permit_must_arrive_before_ready() {
        let mut permits = group(2);
        let first = permits.next().unwrap();
        let second = permits.next().unwrap();
        let reader = first.reader();
        assert!(permits.next().is_none());
        drop(permits);
        second.complete();
        assert_eq!(reader.status(), CacheBootStatus::Pending);
        first.complete();
        assert_eq!(reader.status(), CacheBootStatus::Ready);
    }

    #[test]
    fn dropped_issued_or_unissued_participant_fails_the_group() {
        for issue_second in [false, true] {
            let mut permits = group(2);
            let first = permits.next().unwrap();
            let reader = first.reader();
            first.complete();
            if issue_second {
                drop(permits.next().unwrap());
            }
            drop(permits);
            assert_eq!(reader.status(), CacheBootStatus::Failed);
        }
    }

    #[test]
    fn completion_is_visible_across_threads() {
        let mut permits = group(2);
        let first = permits.next().unwrap();
        let second = permits.next().unwrap();
        let reader = first.reader();
        first.complete();
        assert_eq!(reader.status(), CacheBootStatus::Pending);
        std::thread::spawn(move || second.complete()).join().unwrap();
        assert_eq!(reader.status(), CacheBootStatus::Ready);
    }
}
