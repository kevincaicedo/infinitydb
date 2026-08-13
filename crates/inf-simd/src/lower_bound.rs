//! Branchless lower-bound over sorted `u64` prefix arrays — the B+-tree
//! leaf/branch search primitive (M4.5-S01, ADR-0072 D9's `inf-simd`
//! leaf-kernel scope).
//!
//! The ordered map stores each node's key prefixes as one contiguous
//! big-endian `u64` array (SoA), so "find the first entry ≥ probe" is a
//! count of elements `< probe` over a short sorted slice (≤ the node
//! fanout, 32–64).
//!
//! **A/B disposition (M4.5-S01, dev-tier, 10M-entry trees —
//! `.artifacts/m4.5/s01/`): the explicit-intrinsics kernel was measured
//! and rejected** (the M0-S14 rule — a losing A/B is recorded, not
//! merged). The hand-written AVX2/SSE4.2 paths (sign-flip `pcmpgtq` +
//! movemask popcount behind the CRLF-pattern `AtomicU8` dispatch) lost
//! to this plain count-loop at every probe row — 210 ns vs 270 ns at
//! the 10k hot set, 298 ns vs 373 ns at 100k (fanout 64) — because
//! LLVM already auto-vectorizes the count-loop with vector-accumulate
//! (no per-chunk movemask/popcount), and `#[target_feature]` functions
//! cannot inline into their callers, so the dispatched kernel paid a
//! call + dispatch per tree level. What remains is the winning form:
//! one safe loop, inlinable, auto-vectorized on x86-64 and aarch64
//! alike. Re-evaluating explicit kernels (e.g. AVX-512 on a reference
//! box) starts from those artifact rows.

/// First index `i` in the sorted slice with `sorted[i] >= probe`
/// (`sorted.len()` when every element is smaller). Callers pass one
/// node's live prefix region, so `len` is bounded by the tree fanout.
///
/// Branchless by construction — the loop body is a compare-accumulate
/// with no data-dependent exit, which is what lets LLVM vectorize it.
#[inline]
pub fn lower_bound_u64(sorted: &[u64], probe: u64) -> usize {
    debug_assert!(sorted.windows(2).all(|w| w[0] <= w[1]), "input must be sorted");
    scalar_lower_bound_u64(sorted, probe)
}

/// The count-of-elements-below-probe form (also the property-test
/// oracle for any future explicit kernel).
pub fn scalar_lower_bound_u64(sorted: &[u64], probe: u64) -> usize {
    sorted.iter().map(|&v| usize::from(v < probe)).sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn empty_and_bounds() {
        assert_eq!(lower_bound_u64(&[], 42), 0);
        assert_eq!(lower_bound_u64(&[1, 2, 3], 0), 0);
        assert_eq!(lower_bound_u64(&[1, 2, 3], 4), 3);
        assert_eq!(lower_bound_u64(&[1, 2, 3], 2), 1);
        // Duplicates: first index of the equal run.
        assert_eq!(lower_bound_u64(&[5, 7, 7, 7, 9], 7), 1);
        // Unsigned order across the sign bit.
        assert_eq!(lower_bound_u64(&[1, u64::MAX], u64::MAX), 1);
        assert_eq!(lower_bound_u64(&[0, 1 << 63], (1 << 63) - 1), 1);
    }

    proptest! {
        // Matches `partition_point` on sorted inputs of every length up
        // to twice the largest fanout, including high-bit values.
        #[test]
        fn matches_partition_point(
            mut values in prop::collection::vec(any::<u64>(), 0..129),
            probe: u64,
        ) {
            values.sort_unstable();
            let want = values.partition_point(|&v| v < probe);
            prop_assert_eq!(lower_bound_u64(&values, probe), want);
        }
    }
}
