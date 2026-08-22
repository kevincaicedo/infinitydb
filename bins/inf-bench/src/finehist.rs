//! Client-side latency histogram at **256 linear sub-buckets per octave**
//! (≈ 0.4 % relative resolution) — the `inf-bench` instrument for gates
//! that bind on a ratio of p50s (M4.5-S35's 4-cell ÷ 1-cell rows, ADR-0087
//! D8 as amended 2026-08-22).
//!
//! Why not the kernel's `LogHistogram`: that one is cell-resident and
//! sized for the reactor loop (32 sub-buckets per octave, ≈ 3 %; 15 KiB).
//! A 3 % instrument quantizes a 0.5–1 ms p50 to 16 µs steps, and the
//! S35 4c/1c ratio read 1.28 / 1.31 / 1.35 / 1.38 across campaigns with
//! the 4-cell p50 pinned — the denominator moved by exactly one bucket
//! each time, and the gate's whole margin (≤ 1.3) is one bucket wide.
//! This histogram is bench-only (never on a cell), so it can afford 112 KiB
//! per connection and a 2 µs step at the 512–1024 µs octave: the
//! quantization is ~8× finer than the gate margin instead of equal to it.
//!
//! Same conventions as `LogHistogram` so readings stay comparable: a
//! percentile reports its bucket's **upper bound** (≤ 0.4 % above the true
//! value), clamped to the exact recorded max; `max` is exact; the running
//! `sum` is exact (saturating). Previous-campaign readings taken with the
//! 3 % instrument are up to ~3 % *higher* than this one would report for
//! the same samples — disclosed in every row note that cites a ratio.

const SUB_BITS: u32 = 8;
const SUB: usize = 1 << SUB_BITS; // 256 linear sub-buckets per octave
const BUCKETS: usize = (64 - SUB_BITS as usize) * SUB;

pub struct FineHistogram {
    counts: Box<[u64]>,
    count: u64,
    max: u64,
    sum: u64,
}

impl FineHistogram {
    pub fn new() -> FineHistogram {
        FineHistogram { counts: vec![0u64; BUCKETS].into_boxed_slice(), count: 0, max: 0, sum: 0 }
    }

    #[inline]
    fn index_of(value: u64) -> usize {
        if value < SUB as u64 {
            return value as usize;
        }
        let exp = 63 - value.leading_zeros();
        let sub = ((value >> (exp - SUB_BITS)) & (SUB as u64 - 1)) as usize;
        (exp - SUB_BITS + 1) as usize * SUB + sub
    }

    /// Upper bound of the value range covered by bucket `index`.
    fn bucket_upper(index: usize) -> u64 {
        if index < SUB {
            return index as u64;
        }
        let block = (index / SUB) as u32;
        let sub = (index % SUB) as u64;
        let exp = block + SUB_BITS - 1;
        let width = 1u64 << (exp - SUB_BITS);
        (1u64 << exp) + (sub + 1) * width - 1
    }

    #[inline]
    pub fn record(&mut self, value: u64) {
        self.counts[Self::index_of(value)] += 1;
        self.count += 1;
        self.max = self.max.max(value);
        self.sum = self.sum.saturating_add(value);
    }

    #[cfg(test)]
    pub fn count(&self) -> u64 {
        self.count
    }

    pub fn max(&self) -> u64 {
        self.max
    }

    /// Exact mean of every recorded value (0 on an empty histogram) —
    /// disclosed beside the percentiles, never a gate input on its own
    /// (device tails make means the less reliable statistic).
    pub fn mean(&self) -> f64 {
        if self.count == 0 { 0.0 } else { self.sum as f64 / self.count as f64 }
    }

    /// Value at percentile `p` in `0.0..=100.0`; 0 on an empty histogram.
    /// Reported with the bucket's upper bound (≤ 0.4 % above the true
    /// value), clamped to the exact recorded max.
    pub fn percentile(&self, p: f64) -> u64 {
        if self.count == 0 {
            return 0;
        }
        let rank = ((p / 100.0) * self.count as f64).ceil().max(1.0) as u64;
        let mut seen = 0u64;
        for (i, &c) in self.counts.iter().enumerate() {
            seen += c;
            if seen >= rank {
                return Self::bucket_upper(i).min(self.max);
            }
        }
        self.max
    }

    pub fn merge(&mut self, other: &FineHistogram) {
        for (a, b) in self.counts.iter_mut().zip(other.counts.iter()) {
            *a += b;
        }
        self.count += other.count;
        self.max = self.max.max(other.max);
        self.sum = self.sum.saturating_add(other.sum);
    }
}

impl Default for FineHistogram {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_values_are_exact() {
        let mut h = FineHistogram::new();
        for v in 0..256u64 {
            h.record(v);
        }
        assert_eq!(h.percentile(100.0), 255);
        assert_eq!(h.percentile(1.0), 2);
        assert_eq!(h.count(), 256);
    }

    #[test]
    fn bucket_bounds_tile_the_range_without_gaps() {
        // Every value maps to a bucket whose upper bound is ≥ the value
        // and whose predecessor's upper bound is < the value.
        for &v in
            &[256u64, 257, 511, 512, 513, 543, 559, 575, 735, 751, 1023, 1024, 65_535, 1 << 40]
        {
            let i = FineHistogram::index_of(v);
            assert!(FineHistogram::bucket_upper(i) >= v, "{v}");
            assert!(FineHistogram::bucket_upper(i - 1) < v, "{v}");
        }
    }

    #[test]
    fn resolution_at_the_s35_octave_is_two_microseconds() {
        // 512–1024 µs: 256 buckets of width 2 — the S35 p50s (543–751 µs)
        // quantize at 2 µs, not the kernel histogram's 16.
        let i = FineHistogram::index_of(735);
        assert_eq!(FineHistogram::bucket_upper(i) - FineHistogram::bucket_upper(i - 1), 2);
        let mut h = FineHistogram::new();
        h.record(735);
        assert!(h.percentile(50.0) <= 735 + 2 && h.percentile(50.0) >= 735);
    }

    #[test]
    fn percentile_is_upper_bound_clamped_to_max() {
        let mut h = FineHistogram::new();
        for v in [1000u64, 2000, 3000, 4000] {
            h.record(v);
        }
        assert_eq!(h.max(), 4000);
        assert_eq!(h.percentile(100.0), 4000);
        let p50 = h.percentile(50.0);
        assert!((2000..=2008).contains(&p50), "{p50}");
        assert!((h.mean() - 2500.0).abs() < 1e-9);
    }

    #[test]
    fn merge_adds_counts_and_keeps_max() {
        let mut a = FineHistogram::new();
        let mut b = FineHistogram::new();
        a.record(100);
        b.record(900);
        b.record(901);
        a.merge(&b);
        assert_eq!(a.count(), 3);
        assert_eq!(a.max(), 901);
        assert!((a.mean() - (1901.0 / 3.0)).abs() < 1e-9);
    }
}
