//! A tiered position's retirement list (ADR-0156 D1, D4): the owners of the
//! tier files a covering swap retired, or a dropped namespace's pinned
//! catalog files, each kept, meta and handle together, until a consumed
//! `Unlinked` or `AlreadyAbsent`. One `Vec` split at `ahead_len`: `ahead`
//! is the current pass's unvisited owners, `behind` the rest.
//!
//! | Operation | Effect |
//! |---|---|
//! | push (a commit, a pinned catalog file, a consumed `Retry`) | onto `behind`'s end |
//! | move (`ahead`'s last pinned or not yet due) | `ahead_len − 1`: it becomes `behind`'s first |
//! | offer (`ahead`'s last eligible) | swapped with the last, popped; `ahead_len − 1` |
//! | a pass ends (`ahead` empty, `behind` not) | `ahead_len ← len` |
//!
//! Each owner is visited once a pass, and a push never lengthens the pass
//! in progress. The capacity stays at least the owners plus the jobs the
//! list's owners have out (and, Draining, its catalog): the commit and the
//! drop reserve it, so no move, offer or push of the walk reallocates.

use inf_foundation::time::Nanos;
use inf_log::TierFileMeta;

/// One retiring owner: the file's meta, its handle until it is offered (a
/// retried owner's was closed by its failed attempt), the injected time it
/// is eligible from, and the bytes it charges to its namespace's disk use.
pub(super) struct RetiringFile<H> {
    pub(super) meta: TierFileMeta,
    pub(super) handle: Option<H>,
    pub(super) due: Nanos,
    pub(super) bytes: u64,
}

pub(super) struct RetireList<H> {
    owners: Vec<RetiringFile<H>>,
    ahead_len: usize,
    /// The bytes every owner of the list charges.
    bytes: u64,
}

impl<H> Default for RetireList<H> {
    fn default() -> RetireList<H> {
        RetireList { owners: Vec::new(), ahead_len: 0, bytes: 0 }
    }
}

impl<H> RetireList<H> {
    /// Owners on the list.
    pub(super) fn len(&self) -> usize {
        self.owners.len()
    }

    pub(super) fn is_empty(&self) -> bool {
        self.owners.is_empty()
    }

    /// The bytes the list's owners charge.
    pub(super) fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Room for `more` owners beside those held and the `out` jobs whose
    /// retries may come back, reserved once by the commit or the drop.
    pub(super) fn reserve(&mut self, more: usize, out: usize) {
        self.owners.reserve(more.saturating_add(out));
    }

    /// Pushes `owner` onto `behind`'s end.
    pub(super) fn push(&mut self, owner: RetiringFile<H>) {
        debug_assert!(
            self.owners.len() < self.owners.capacity(),
            "a retiring owner's push fits the reserved list"
        );
        self.bytes += owner.bytes;
        self.owners.push(owner);
    }

    /// `ahead`'s last owner, the pass's next.
    pub(super) fn ahead_last(&self) -> Option<&RetiringFile<H>> {
        self.ahead_len.checked_sub(1).map(|at| &self.owners[at])
    }

    /// `ahead`'s last owner moves onto `behind`, its first.
    pub(super) fn move_behind(&mut self) {
        debug_assert!(self.ahead_len > 0, "a move takes ahead's last");
        self.ahead_len -= 1;
    }

    /// `ahead`'s last owner, taken for its offer: swapped with the list's
    /// last, which lands first in `behind`.
    pub(super) fn take_ahead_last(&mut self) -> Option<RetiringFile<H>> {
        let at = self.ahead_len.checked_sub(1)?;
        let owner = self.owners.swap_remove(at);
        self.ahead_len = at;
        self.bytes -= owner.bytes;
        Some(owner)
    }

    /// Ends a pass once `ahead` is empty: every owner is `ahead` again.
    /// `false` with `ahead` not yet empty or nothing on the list.
    pub(super) fn end_pass(&mut self) -> bool {
        if self.ahead_len > 0 || self.owners.is_empty() {
            return false;
        }
        self.ahead_len = self.owners.len();
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use inf_store::LogicalAddr;

    fn owner(id: u32) -> RetiringFile<()> {
        let meta = TierFileMeta {
            id,
            base: LogicalAddr::ZERO,
            data_len: 0,
            reason: inf_log::SealReason::Capacity,
            path: std::path::PathBuf::from(format!("tier-{id}")),
        };
        RetiringFile { meta, handle: None, due: Nanos(0), bytes: 10 }
    }

    /// A pass visits each owner once, last first; a move and an offer both
    /// shorten `ahead`; a push lands in `behind`, never in the pass under
    /// way; the pass's end makes every owner `ahead` again; the bytes follow
    /// the owners held.
    #[test]
    fn a_pass_visits_each_owner_once_and_a_push_waits_for_the_next() {
        let mut list = RetireList::default();
        list.reserve(4, 0);
        for id in 0..3 {
            list.push(owner(id));
        }
        assert!(list.end_pass(), "the first pass begins over every owner");
        assert_eq!(list.ahead_last().map(|o| o.meta.id), Some(2));
        list.move_behind();
        let taken = list.take_ahead_last().expect("owner 1");
        assert_eq!(taken.meta.id, 1);
        list.push(owner(3));
        assert_eq!(list.ahead_last().map(|o| o.meta.id), Some(0), "the push is not in the pass");
        list.move_behind();
        assert!(list.ahead_last().is_none(), "ahead is empty");
        assert!(list.end_pass(), "the pass ends with owners behind");
        let mut next: Vec<u32> = Vec::new();
        while let Some(owner) = list.take_ahead_last() {
            next.push(owner.meta.id);
        }
        next.sort_unstable();
        assert_eq!(next, [0, 2, 3], "the next pass holds every owner left");
        assert_eq!(list.bytes(), 0);
        assert!(!list.end_pass(), "an empty list begins no pass");
    }
}
