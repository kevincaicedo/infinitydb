//! Capped containers for cell code (ADR-0163 D2, ADR-0151 D2). A cell-code
//! container grows only through a reservation or a named merge, over a
//! [`Cap`] declared as a `const` of its crate's `limits` module, and every
//! instance registers a row of its cell's [`CapCensus`]. Safe Rust; each
//! outcome is an enum, never a bool or an `Option` with three meanings.
//!
//! The capacity machine every capped type shares: a reservation is an
//! exclusive borrow, so at most one exists and it cannot outlive its step;
//! `Full` holds no value and changes nothing but the row's crossings; a
//! publish cannot allocate or refuse; no allocator call follows `Live`.
//! There is no `Deref` to a std type, no `extend`, no `From<std>`, no
//! `clear`, `retain` or `entry`: nothing grows around admission.
#![cfg_attr(
    not(test),
    deny(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_possible_wrap,
        clippy::arithmetic_side_effects
    )
)]

use core::fmt;
use core::marker::PhantomData;
use core::num::NonZeroU32;

mod census;
mod deque;

pub use census::{CapCensus, CapRow, CensusPhase};
pub use deque::{CappedDeque, DequeSlot, Reserved};

/// When a container's host fills it: while the cell serves, or whole before
/// the cell's serve mark (ADR-0163 D2). The census keeps the fill on the row
/// and holds an `Assembly` row to it at the mark.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CapFill {
    /// Filled while serving: the row's `high_water` rises as the site
    /// reserves, and a cap one short of the site's demand shows as a
    /// crossing.
    Serving,
    /// Filled whole before the serve mark, one instance per cell, and takes
    /// no publish, crossing or registration after it: a slab's or a map's.
    /// A deque refuses this fill: no site fills a deque before serving. The
    /// census checks the row at the mark by its live count, never by its
    /// high water, which a removal before the mark leaves at the cap.
    Assembly,
}

/// A capacity: its name, its entries and its fill, declared as a `const` of
/// a `limits` module whose doc comment states the unit, the owner and the
/// crossing. `Cap::entries` is spelled only in a `limits.rs` (the lint-scopes
/// gate holds that), so a cap that is only a literal cannot exist, and its
/// entries are a const generic, so a zero cap fails to compile.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Cap {
    name: &'static str,
    entries: NonZeroU32,
    fill: CapFill,
}

impl Cap {
    /// `N` entries under `name`, filled as `fill` says. A zero `N` is refused
    /// when the call is compiled, never at run time.
    #[must_use]
    pub const fn entries<const N: u32>(name: &'static str, fill: CapFill) -> Cap {
        let entries = const {
            match NonZeroU32::new(N) {
                Some(entries) => entries,
                None => panic!("a cap is at least one entry"),
            }
        };
        Cap { name, entries, fill }
    }

    /// The row's name on the cell's census.
    #[must_use]
    pub const fn name(self) -> &'static str {
        self.name
    }

    /// The most entries the container holds at once; reaching it is legal.
    #[must_use]
    pub const fn entries_max(self) -> u32 {
        self.entries.get()
    }

    /// When the container's host fills it: while serving, or whole before
    /// the cell's serve mark (`CapFill`); the census keeps it on the row.
    #[must_use]
    pub const fn fill(self) -> CapFill {
        self.fill
    }
}

/// Why a capped container was not built: the type returns it and builds
/// nothing, so a refused construction serves nothing and leaves nothing
/// charged beyond the census row its name keeps (ADR-0151 D3).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CapError {
    /// The allocator refused the backing.
    Alloc,
    /// `entries × stride` passes the backing's byte bound, or the shape is
    /// not representable on this host.
    Layout,
    /// The census refused the row: a name registered with a second cap value
    /// or a second fill, an `Assembly` name registered twice, a name past
    /// `CAP_CENSUS_ROWS_MAX`, or an `Assembly` cap given to a deque.
    Census,
}

impl fmt::Display for CapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            CapError::Alloc => "the allocator refused the backing",
            CapError::Layout => "entries times stride passes the backing's byte bound",
            CapError::Census => "the cell's cap census refused the row",
        })
    }
}

impl std::error::Error for CapError {}

/// A payload that may be merged or dropped at a cap (ADR-0151 D2's
/// coalescing row). A type opts in by name, and a reply, a pin or a
/// descriptor never does; the lint-scopes gate prints every `impl Lossy for`
/// site, so each opt-in is a reviewed line.
pub trait Lossy {}

/// The merge a `Coalesce` deque performs at its cap: `new` folds into
/// `back`, which stays the deque's last entry.
pub trait Merge<T: Lossy> {
    fn merge(back: &mut T, new: T);
}

/// The pacing or refusing crossing (ADR-0151 D2): growth is `reserve()`,
/// then `publish` on the slot; `Full` takes no value.
pub struct Reserve;

/// The coalescing crossing (ADR-0151 D2): growth is `push`, which merges
/// into the back under `M` at the cap and counts the merge.
pub struct Coalesce<M>(PhantomData<M>);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cap_carries_its_name_entries_and_fill() {
        const CAP: Cap = Cap::entries::<16>("marks", CapFill::Serving);
        assert_eq!(CAP.name(), "marks");
        assert_eq!(CAP.entries_max(), 16);
        assert_eq!(CAP.fill(), CapFill::Serving);
        const ONE: Cap = Cap::entries::<1>("one", CapFill::Assembly);
        assert_eq!(ONE.entries_max(), 1);
        assert_eq!(ONE.fill(), CapFill::Assembly);
    }

    #[test]
    fn errors_name_their_cause() {
        assert_eq!(CapError::Alloc.to_string(), "the allocator refused the backing");
        assert!(CapError::Layout.to_string().contains("byte bound"));
        assert!(CapError::Census.to_string().contains("census"));
    }
}
