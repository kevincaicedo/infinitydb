//! Owned allocation charges used by bounded compiled caches (ADR-0146).

use std::alloc::Layout;

/// Requested storage for an Rc-owned value, including its reference counts
/// and alignment padding. This matches the pinned compiler's alloc::rc::RcInner:
/// two usize counters followed by the value. Cache allocation-census tests
/// must detect layout drift when the toolchain changes. Allocator-private
/// bookkeeping is excluded. An unrepresentable layout saturates the charge,
/// so bounded cache admission refuses it.
pub fn rc_allocation_bytes<T: ?Sized>(value: &T) -> usize {
    Layout::new::<[usize; 2]>()
        .extend(Layout::for_value(value))
        .map_or(usize::MAX, |(layout, _)| layout.pad_to_align().size())
}
