//! The tier round's span selection (ADR-0155 D1): from the flush cursor,
//! the largest prefix of whole records whose bytes fit the slice, or the
//! first record alone when it is larger, ending at or before the hard
//! bound (a hole's start, the ro-boundary or the ring top). Records are
//! found by their headers' lengths, never by scanning payloads and never
//! by a recorded seal cut, which is a hint and no authority to exceed the
//! slice. A selected span is at most `max(S, M)` bytes.

/// The end of the span selected at `start` (ADR-0155 D1): records are
/// taken while their bytes from `start` fit `slice_bytes`, the first
/// whatever its length, and none past `hard_end`. `record_len_at(addr)`
/// decodes the bounded header at `addr` into the record's encoded length.
/// `None` when the first record is empty or crosses `hard_end`: the bytes
/// below the bound are not whole records. Total and iterative: each
/// header visited advances by its nonzero length, so at most
/// `slice_bytes / 8 + 1` headers are visited (a record is at least its
/// 8-byte header).
#[must_use]
pub fn select_whole_records(
    start: u64,
    hard_end: u64,
    slice_bytes: u64,
    mut record_len_at: impl FnMut(u64) -> u64,
) -> Option<u64> {
    let first = record_len_at(start);
    let mut end = start.checked_add(first).filter(|&end| first > 0 && end <= hard_end)?;
    // The span may end at `start + slice_bytes`; past `u64` every end fits.
    let slice_end = start.checked_add(slice_bytes);
    while end < hard_end {
        let len = record_len_at(end);
        let Some(next) = end.checked_add(len).filter(|&next| len > 0 && next <= hard_end) else {
            break;
        };
        if slice_end.is_some_and(|limit| next > limit) {
            break;
        }
        end = next;
    }
    Some(end)
}

#[cfg(test)]
mod tests {
    use super::select_whole_records;
    use inf_foundation::rng::{Entropy, SplitMix64};

    /// The independent oracle (the probe's prefix-sum model): the largest
    /// prefix of whole records whose sum fits the slice, by bisection over
    /// complete prefix sums, or the first record alone; never past the
    /// hard end.
    fn oracle(lengths: &[u64], hard_end: u64, slice: u64) -> Option<u64> {
        let mut sums = vec![0u64];
        for &len in lengths {
            let next = sums.last().copied().unwrap_or(0) + len;
            if len == 0 || next > hard_end {
                break;
            }
            sums.push(next);
        }
        if sums.len() == 1 {
            return None;
        }
        let fit = sums.partition_point(|&sum| sum <= slice);
        Some(sums[fit.saturating_sub(1).max(1)])
    }

    fn select(lengths: &[u64], hard_end: u64, slice: u64) -> Option<u64> {
        let mut at = Vec::with_capacity(lengths.len());
        let mut addr = 0u64;
        for &len in lengths {
            at.push(addr);
            addr += len;
        }
        select_whole_records(0, hard_end, slice, |addr| {
            at.iter().position(|&start| start == addr).map_or(0, |i| lengths[i])
        })
    }

    /// The selector against the prefix-sum oracle over every sequence of
    /// one to four records of 1..=8 bytes, every slice 1..=16 and every
    /// record boundary as the hard end (the probe's exhaustive domain).
    #[test]
    fn the_selector_equals_the_prefix_sum_oracle_exhaustively() {
        let mut cases = 0u64;
        for count in 1..=4u32 {
            for code in 0..8u64.pow(count) {
                let lengths: Vec<u64> = (0..count).map(|i| (code / 8u64.pow(i)) % 8 + 1).collect();
                let total: u64 = lengths.iter().sum();
                let mut bounds = vec![];
                let mut sum = 0;
                for &len in &lengths {
                    sum += len;
                    bounds.push(sum);
                }
                for &hard in &bounds {
                    for slice in 1..=16 {
                        let got = select(&lengths, hard, slice);
                        assert_eq!(
                            got,
                            oracle(&lengths, hard, slice),
                            "{lengths:?} {hard} {slice}"
                        );
                        let end = got.expect("a whole first record");
                        assert!(end <= hard.min(total));
                        assert!(end <= slice.max(lengths[0]), "A <= max(S, the first record)");
                        cases += 1;
                    }
                }
            }
        }
        assert!(cases > 10_000);
    }

    /// Seeded histories of large and tiny records, empty and crossing
    /// records included: the selector never visits more headers than the
    /// slice's bound and agrees with the oracle.
    #[test]
    fn seeded_histories_agree_with_the_oracle_within_the_visit_bound() {
        let mut rng = SplitMix64::new(0x7135_5E1E);
        for _ in 0..2_000 {
            let count = 1 + rng.next_below(64) as usize;
            let lengths: Vec<u64> = (0..count)
                .map(|_| match rng.next_below(10) {
                    0 => 0,
                    1 => 1 << 20,
                    _ => 8 + rng.next_below(4096),
                })
                .collect();
            let total: u64 = lengths.iter().sum();
            let hard = rng.next_below(total + 2);
            let slice = 1 + rng.next_below(64 << 10);
            let mut visits = 0u64;
            let mut at = Vec::new();
            let mut addr = 0u64;
            for &len in &lengths {
                at.push(addr);
                addr += len;
            }
            let got = select_whole_records(0, hard, slice, |addr| {
                visits += 1;
                at.iter().position(|&start| start == addr).map_or(0, |i| lengths[i])
            });
            assert_eq!(got, oracle(&lengths, hard, slice), "{lengths:?} {hard} {slice}");
            assert!(visits <= slice / 8 + 2, "{visits} header visits for a {slice} B slice");
        }
    }
}
