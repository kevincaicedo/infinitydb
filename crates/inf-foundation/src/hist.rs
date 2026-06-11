//! Always-on log-linear histogram (HDR-style) for loop-iteration and latency
//! tracking. Fixed memory, no allocation on `record`, ~3% relative error
//! (32 sub-buckets per power of two). Cell-local: not thread-safe by design.

const SUB_BITS: u32 = 5;
const SUB: usize = 1 << SUB_BITS; // 32 linear sub-buckets per octave
const BUCKETS: usize = (64 - SUB_BITS as usize) * SUB; // covers full u64 range

pub struct LogHistogram {
    counts: Box<[u64; BUCKETS]>,
    count: u64,
    max: u64,
}

impl LogHistogram {
    pub fn new() -> LogHistogram {
        LogHistogram { counts: Box::new([0; BUCKETS]), count: 0, max: 0 }
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
    }

    pub fn count(&self) -> u64 {
        self.count
    }

    pub fn max(&self) -> u64 {
        self.max
    }

    /// Value at percentile `p` in `0.0..=100.0`. Returns 0 on an empty histogram.
    /// Reported with the bucket's upper bound (≤ ~3% above the true value),
    /// clamped to the exact recorded max.
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

    pub fn merge(&mut self, other: &LogHistogram) {
        for (a, b) in self.counts.iter_mut().zip(other.counts.iter()) {
            *a += b;
        }
        self.count += other.count;
        self.max = self.max.max(other.max);
    }

    pub fn clear(&mut self) {
        self.counts.fill(0);
        self.count = 0;
        self.max = 0;
    }
}

impl Default for LogHistogram {
    fn default() -> Self {
        Self::new()
    }
}

impl core::fmt::Debug for LogHistogram {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "LogHistogram {{ count: {}, p50: {}, p99: {}, p999: {}, max: {} }}",
            self.count,
            self.percentile(50.0),
            self.percentile(99.0),
            self.percentile(99.9),
            self.max
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_values_are_exact() {
        let mut h = LogHistogram::new();
        for v in 0..32u64 {
            h.record(v);
        }
        assert_eq!(h.percentile(100.0), 31);
        assert_eq!(h.percentile(1.0), 0);
    }

    #[test]
    fn relative_error_is_bounded() {
        let mut h = LogHistogram::new();
        for v in [100u64, 1_000, 10_000, 1_000_000, 123_456_789] {
            h.clear();
            h.record(v);
            let got = h.percentile(50.0);
            let err = got.abs_diff(v) as f64 / v as f64;
            assert!(err <= 0.04, "value {v}: reported {got}, err {err}");
        }
    }

    #[test]
    fn percentiles_are_ordered() {
        let mut h = LogHistogram::new();
        let mut x = 1u64;
        for _ in 0..10_000 {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            h.record(x >> 40);
        }
        let p50 = h.percentile(50.0);
        let p99 = h.percentile(99.0);
        let p999 = h.percentile(99.9);
        assert!(p50 <= p99 && p99 <= p999 && p999 <= h.max());
    }

    #[test]
    fn merge_accumulates() {
        let mut a = LogHistogram::new();
        let mut b = LogHistogram::new();
        a.record(10);
        b.record(1_000);
        a.merge(&b);
        assert_eq!(a.count(), 2);
        assert_eq!(a.max(), 1_000);
    }
}
