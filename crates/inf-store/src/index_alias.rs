//! The alias-group enumeration (ADR-0139 D2 rules 2 and 5, D9): "who
//! else has this primary-key hash". A tree entry `(k, h)` is a fact
//! about the whole group `G(h)` — every physically present record whose
//! full key hashes to `h` — so nothing may be removed, or resolved, on
//! `h` without first seeing the group.
//!
//! This module is the **only** place an [`AliasView`] is built, and the
//! tree's `remove` takes one: a removal that skipped the enumeration does
//! not compile. The walk is bounded three ways (`limits`), inside the
//! traversal; an unfinished walk yields [`AliasWalk::Over`], which
//! carries no view — an unanswered question decides nothing.

#[cfg(any(test, feature = "doc"))]
use core::ops::ControlFlow;

use inf_alloc::ArenaAddr;

#[cfg(any(test, feature = "doc"))]
use crate::index::{Index, ProbeEnd};
use crate::limits::IDX_ALIAS_GROUP_MAX;
#[cfg(any(test, feature = "doc"))]
use crate::limits::{IDX_ALIAS_REHASH_MAX, IDX_ALIAS_WALK_GROUPS_MAX};
use crate::ordered::PkRef;

/// Which budget an enumeration crossed (one crossing behavior for all
/// three; the reason is for the operator).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum AliasLimit {
    /// A chain longer than `IDX_ALIAS_WALK_GROUPS_MAX` control groups.
    Groups,
    /// More than `IDX_ALIAS_REHASH_MAX` fragment matches to fetch.
    Rehashes,
    /// More than `IDX_ALIAS_GROUP_MAX` confirmed members.
    Members,
}

/// The members of one alias group, minus whoever the caller excluded
/// (the dying record, or the bracket's write set). Holding one proves a
/// **complete** enumeration ran for [`pk_ref`](Self::pk_ref).
#[derive(Debug)]
#[cfg_attr(
    not(any(test, feature = "doc")),
    allow(
        dead_code,
        reason = "a slim build has no index trees: nothing enumerates or reads a view"
    )
)]
pub struct AliasView {
    pk_ref: PkRef,
    /// `Some` for the first `count` slots — a fixed array, so the common
    /// one-member group costs no allocation.
    members: [Option<ArenaAddr>; IDX_ALIAS_GROUP_MAX],
    count: usize,
}

impl AliasView {
    /// The hash this view enumerated.
    #[must_use]
    pub fn pk_ref(&self) -> PkRef {
        self.pk_ref
    }

    /// The confirmed members, in probe order.
    #[cfg(any(test, feature = "doc"))]
    pub(crate) fn members(&self) -> impl Iterator<Item = ArenaAddr> + '_ {
        self.members[..self.count].iter().flatten().copied()
    }

    /// True when no other record shares the ref — the common case.
    #[cfg(feature = "doc")]
    pub(crate) fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// A view with no members, for code that drives a tree with no
    /// record table behind it (ordered-map suites and benches). Absent
    /// from shipping builds (`check-shipping-features.sh`).
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn unaliased_for_tests(pk_ref: PkRef) -> AliasView {
        AliasView::empty(pk_ref)
    }

    #[cfg(any(test, feature = "doc", feature = "test-support"))]
    fn empty(pk_ref: PkRef) -> AliasView {
        AliasView { pk_ref, members: [None; IDX_ALIAS_GROUP_MAX], count: 0 }
    }
}

/// One enumeration's outcome. `Over` means the question could not be
/// answered within the budgets, so it carries nothing to decide on.
#[derive(Debug)]
pub enum AliasWalk {
    Complete(AliasView),
    Over(AliasLimit),
}

/// What one enumeration cost — the traversal's own counts, not the
/// callback's (the falsifier and the walk-bounds tests read these).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct AliasTally {
    /// Control groups loaded.
    pub groups: u32,
    /// Fragment matches whose record was fetched — every one, whatever
    /// the inspection then found (`IDX_ALIAS_REHASH_MAX` bounds this).
    pub fetches: u32,
}

/// What inspecting one fragment match found. An inspection is the one
/// place an enumeration reads a record, so it is what the fetch budget
/// charges — an excluded record costs a fetch like any other.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[cfg(any(test, feature = "doc"))]
pub(crate) enum Candidate {
    /// The caller's own record (a write-set key, by full key): not a
    /// view member.
    Excluded,
    /// The full keyed hash equals the enumerated one.
    Alias,
    /// Matched the table's 22-bit fragment only.
    Neighbour,
}

/// Enumerates `G(hash) ∖ excluded` over a memory table. The table
/// filters on 22 bits only, so `inspect(addr)` fetches the record and
/// says what it is; every call is charged to `IDX_ALIAS_REHASH_MAX`
/// **before** it runs, so the budget bounds record fetches, not just
/// keyed hashes (ADR-0139 D9). `skip(addr)` is for an exclusion that
/// needs no record — the death sites' dying address — and is free.
#[cfg(any(test, feature = "doc"))]
pub(crate) fn alias_view(
    index: &Index,
    hash: u64,
    mut skip: impl FnMut(ArenaAddr) -> bool,
    mut inspect: impl FnMut(ArenaAddr) -> Candidate,
) -> (AliasWalk, AliasTally) {
    let mut view = AliasView::empty(PkRef::from_key_hash(hash));
    let mut tally = AliasTally::default();
    if cfg!(inf_canary_alias_blind) {
        return (AliasWalk::Complete(view), tally);
    }
    let unbounded = cfg!(inf_canary_walk_unbounded);
    let groups_max = if unbounded { index.group_count() } else { IDX_ALIAS_WALK_GROUPS_MAX };
    // Read only when the walk ends `Stopped`; both `Break`s set it first.
    let mut stopped_on = AliasLimit::Rehashes;
    let mut charged = 0usize;
    let end = index.probe_exact_bounded(hash, groups_max, |addr| {
        if skip(addr) {
            return ControlFlow::Continue(());
        }
        if charged == IDX_ALIAS_REHASH_MAX && !unbounded {
            stopped_on = AliasLimit::Rehashes;
            return ControlFlow::Break(());
        }
        charged += 1;
        tally.fetches += 1;
        match inspect(addr) {
            Candidate::Excluded => {
                // The plant: an excluded record's fetch is refunded, so
                // a write set of aliases reads records past the budget.
                if cfg!(inf_canary_fetch_uncharged) {
                    charged -= 1;
                }
                return ControlFlow::Continue(());
            }
            Candidate::Neighbour if !cfg!(inf_canary_alias_no_rehash) => {
                return ControlFlow::Continue(());
            }
            Candidate::Alias | Candidate::Neighbour => {}
        }
        if view.count == IDX_ALIAS_GROUP_MAX {
            if unbounded {
                return ControlFlow::Continue(());
            }
            stopped_on = AliasLimit::Members;
            return ControlFlow::Break(());
        }
        view.members[view.count] = Some(addr);
        view.count += 1;
        ControlFlow::Continue(())
    });
    let walk = match end {
        ProbeEnd::ChainEnd { groups } => {
            tally.groups = groups;
            AliasWalk::Complete(view)
        }
        ProbeEnd::GroupBudget => {
            tally.groups = u32::try_from(groups_max).unwrap_or(u32::MAX);
            AliasWalk::Over(AliasLimit::Groups)
        }
        ProbeEnd::Stopped { groups } => {
            tally.groups = groups;
            AliasWalk::Over(stopped_on)
        }
    };
    (walk, tally)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TARGET: u64 = 0x5EED_0000_0000_0040;

    fn addr(i: u64) -> ArenaAddr {
        ArenaAddr::from_raw(i).expect("small")
    }

    /// A table over planted hashes: `hashes[i]` is the full hash of the
    /// record at address `i` (the index keeps 22 bits of it).
    fn table(hashes: &[u64]) -> Index {
        let mut index: Index = Index::with_capacity(hashes.len().max(64));
        for (i, hash) in hashes.iter().enumerate() {
            index.insert(*hash, addr(i as u64));
        }
        index
    }

    /// Same home group and 22-bit fragment as `TARGET`, different hash.
    fn neighbour(i: u64) -> u64 {
        TARGET ^ ((i + 1) << 20)
    }

    fn by_hash(hashes: &[u64]) -> impl FnMut(ArenaAddr) -> Candidate + '_ {
        |a| match hashes[a.to_raw() as usize] {
            TARGET => Candidate::Alias,
            _ => Candidate::Neighbour,
        }
    }

    /// The death sites' shape: the excluded record is known by address.
    fn enumerate(index: &Index, hashes: &[u64], exclude: Option<u64>) -> (AliasWalk, AliasTally) {
        alias_view(index, TARGET, |a| Some(a.to_raw()) == exclude, by_hash(hashes))
    }

    /// The bracket's shape: exclusion is by full key, so it reads the
    /// record — addresses below `write_set` stand in for write-set keys.
    fn enumerate_bracket(
        index: &Index,
        hashes: &[u64],
        write_set: u64,
    ) -> (AliasWalk, AliasTally, u32) {
        let mut reads = 0u32;
        let mut classify = by_hash(hashes);
        let (walk, tally) = alias_view(
            index,
            TARGET,
            |_| false,
            |a| {
                reads += 1;
                if a.to_raw() < write_set { Candidate::Excluded } else { classify(a) }
            },
        );
        (walk, tally, reads)
    }

    #[test]
    fn a_fragment_neighbour_is_not_an_alias() {
        let hashes = [TARGET, neighbour(0), TARGET, neighbour(1)];
        let index = table(&hashes);
        let (walk, tally) = enumerate(&index, &hashes, Some(0));
        let AliasWalk::Complete(view) = walk else { panic!("within every budget: {walk:?}") };
        let members: Vec<ArenaAddr> = view.members().collect();
        assert_eq!(members, [addr(2)], "the excluded record and both neighbours are out");
        assert_eq!(view.pk_ref(), PkRef::from_key_hash(TARGET));
        assert_eq!(tally.fetches, 3, "every fragment match but the skipped one is fetched");
        assert_eq!(tally.groups, 1);
    }

    #[test]
    fn members_eight_serve_nine_are_over() {
        let eight = [TARGET; IDX_ALIAS_GROUP_MAX];
        let (walk, _) = enumerate(&table(&eight), &eight, None);
        let AliasWalk::Complete(view) = walk else { panic!("eight members fit: {walk:?}") };
        assert_eq!(view.members().count(), IDX_ALIAS_GROUP_MAX);
        let nine = [TARGET; IDX_ALIAS_GROUP_MAX + 1];
        let (walk, _) = enumerate(&table(&nine), &nine, None);
        assert!(matches!(walk, AliasWalk::Over(AliasLimit::Members)), "{walk:?}");
        // The excluded record is not a view member: nine in the group,
        // eight in the view.
        let (walk, _) = enumerate(&table(&nine), &nine, Some(3));
        assert!(matches!(walk, AliasWalk::Complete(_)), "{walk:?}");
    }

    #[test]
    fn rehashes_sixteen_serve_the_seventeenth_is_never_fetched() {
        let sixteen: Vec<u64> = (0..IDX_ALIAS_REHASH_MAX as u64).map(neighbour).collect();
        let (walk, tally) = enumerate(&table(&sixteen), &sixteen, None);
        assert!(matches!(walk, AliasWalk::Complete(_)), "{walk:?}");
        assert_eq!(tally.fetches as usize, IDX_ALIAS_REHASH_MAX);
        let seventeen: Vec<u64> = (0..=IDX_ALIAS_REHASH_MAX as u64).map(neighbour).collect();
        let (walk, tally) = enumerate(&table(&seventeen), &seventeen, None);
        assert!(matches!(walk, AliasWalk::Over(AliasLimit::Rehashes)), "{walk:?}");
        assert_eq!(tally.fetches as usize, IDX_ALIAS_REHASH_MAX, "stopped before the 17th");
    }

    /// The budget is record **fetches**: the bracket excludes by full
    /// key, which reads the candidate, so an excluded record is charged
    /// like any other. Sixteen write-set aliases serve; with a
    /// seventeenth the walk stops before reading it — a budget that
    /// charges only the re-hash reads all seventeen and serves.
    #[test]
    fn an_excluded_record_is_a_charged_fetch() {
        let sixteen = [TARGET; IDX_ALIAS_REHASH_MAX];
        let (walk, tally, reads) = enumerate_bracket(&table(&sixteen), &sixteen, 16);
        let AliasWalk::Complete(view) = walk else { panic!("sixteen fetches fit: {walk:?}") };
        assert_eq!(view.members().count(), 0, "the whole group is the write set");
        assert_eq!((tally.fetches, reads), (16, 16));
        let seventeen = [TARGET; IDX_ALIAS_REHASH_MAX + 1];
        let (walk, tally, reads) = enumerate_bracket(&table(&seventeen), &seventeen, 17);
        assert!(matches!(walk, AliasWalk::Over(AliasLimit::Rehashes)), "{walk:?}");
        assert_eq!(reads as usize, IDX_ALIAS_REHASH_MAX, "the 17th record is never read");
        assert_eq!(tally.fetches, reads, "the tally is the closure's own call count");
    }

    /// The member cap counts the **view** — the group minus whoever the
    /// caller excluded: eight members beside four write-set aliases is a
    /// twelve-record group, and serves.
    #[test]
    fn the_member_cap_applies_after_exclusion() {
        let twelve = [TARGET; IDX_ALIAS_GROUP_MAX + 4];
        let (walk, tally, _) = enumerate_bracket(&table(&twelve), &twelve, 4);
        let AliasWalk::Complete(view) = walk else { panic!("eight view members fit: {walk:?}") };
        assert_eq!(view.members().count(), IDX_ALIAS_GROUP_MAX);
        assert_eq!(tally.fetches, 12);
        let (walk, ..) = enumerate_bracket(&table(&twelve), &twelve, 3);
        assert!(matches!(walk, AliasWalk::Over(AliasLimit::Members)), "{walk:?}");
    }

    /// `full` consecutive full groups on `TARGET`'s chain, none passing
    /// its fragment: only the traversal's own budget can end this walk.
    fn match_free_chain(full: usize) -> (Index, Vec<u64>) {
        let mut index: Index = Index::with_capacity(6_900);
        let mask = index.group_count() - 1;
        let mut hashes = Vec::new();
        let mut group = (TARGET as usize) & mask;
        for stride in 1..=full {
            for _ in 0..crate::index::GROUP {
                let hash = (!TARGET & !(mask as u64)) | group as u64;
                index.insert(hash, addr(hashes.len() as u64));
                hashes.push(hash);
            }
            group = (group + stride) & mask;
        }
        (index, hashes)
    }

    #[test]
    fn groups_thirty_two_serve_thirty_three_are_over() {
        let (index, hashes) = match_free_chain(IDX_ALIAS_WALK_GROUPS_MAX - 1);
        let (walk, tally) = enumerate(&index, &hashes, None);
        assert!(matches!(walk, AliasWalk::Complete(_)), "ends in its 32nd group: {walk:?}");
        assert_eq!(tally.groups as usize, IDX_ALIAS_WALK_GROUPS_MAX);
        let (index, hashes) = match_free_chain(IDX_ALIAS_WALK_GROUPS_MAX);
        let (walk, _) = enumerate(&index, &hashes, None);
        assert!(matches!(walk, AliasWalk::Over(AliasLimit::Groups)), "needs a 33rd: {walk:?}");
    }

    /// The hostile admitted state (80 % occupancy, a 411-group chain):
    /// the walk loads exactly 32 groups and fetches no record. A budget
    /// kept in the callback alone reads 411 here.
    #[test]
    fn the_long_chain_state_loads_exactly_the_group_budget() {
        let (index, hashes) = match_free_chain(410);
        let (walk, tally) = enumerate(&index, &hashes, None);
        assert!(matches!(walk, AliasWalk::Over(AliasLimit::Groups)), "{walk:?}");
        assert_eq!(tally.groups as usize, IDX_ALIAS_WALK_GROUPS_MAX);
        assert_eq!(tally.fetches, 0);
    }
}
