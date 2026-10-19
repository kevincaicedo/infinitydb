//! Always-on log-linear histogram (HDR-style) for loop-iteration and latency
//! tracking. Fixed memory, no allocation on `record`, ~3% relative error
//! (32 sub-buckets per power of two). Cell-local: not thread-safe by design.

const SUB_BITS: u32 = 5;
const SUB: usize = 1 << SUB_BITS; // 32 linear sub-buckets per octave
// The 32 exact slots for `value < SUB`, then one octave per exponent
// `SUB_BITS..=63` — `64 - SUB_BITS` octaves — so `index_of` is total over
// `u64` (F-L16-02: the `+ 1` was missing and every value ≥ 2^63 indexed
// past the array).
const BUCKETS: usize = (64 - SUB_BITS as usize + 1) * SUB;

pub struct LogHistogram {
    counts: Box<[u64; BUCKETS]>,
    count: u64,
    max: u64,
    /// Exact running sum (saturating) — the window-mean input for
    /// tripwires that compare consecutive tick windows (ADR-0086 D7).
    sum: u64,
}

impl LogHistogram {
    /// Schema-1 snapshot width: exact small values and 32 slots per octave.
    pub const BUCKET_COUNT: usize = BUCKETS;

    pub fn new() -> LogHistogram {
        LogHistogram { counts: Box::new([0; BUCKETS]), count: 0, max: 0, sum: 0 }
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
        // Grouped so the top bucket (`exp == 63`, `sub == 31`) lands on
        // `u64::MAX` without passing through 2^64.
        (1u64 << exp) + ((sub + 1) * width - 1)
    }

    /// Total over `u64`: every value has a bucket (`BUCKETS` covers the
    /// exact slots plus every octave through `2^63..=u64::MAX`).
    #[inline]
    pub fn record(&mut self, value: u64) {
        self.counts[Self::index_of(value)] += 1;
        self.count += 1;
        self.max = self.max.max(value);
        self.sum = self.sum.saturating_add(value);
    }

    pub fn count(&self) -> u64 {
        self.count
    }

    /// Cumulative counts; subtract matching snapshots before taking a percentile.
    pub fn bucket_counts(&self) -> &[u64; BUCKETS] {
        &self.counts
    }

    /// Conservative sample bound for a schema-1 bucket; invalid indexes are refused.
    pub fn bucket_upper_bound(index: usize) -> Option<u64> {
        (index < BUCKETS).then(|| Self::bucket_upper(index))
    }

    /// Copy into reusable scrape storage without allocating per request.
    pub fn copy_from(&mut self, source: &Self) {
        self.counts.copy_from_slice(source.counts.as_slice());
        self.count = source.count;
        self.max = source.max;
        self.sum = source.sum;
    }

    /// Exact sum of every recorded value (saturating at `u64::MAX`).
    pub fn sum(&self) -> u64 {
        self.sum
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
        self.sum = self.sum.saturating_add(other.sum);
    }

    pub fn clear(&mut self) {
        self.counts.fill(0);
        self.count = 0;
        self.max = 0;
        self.sum = 0;
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

    /// F-L16-02 (review of 2026-08-30): `record` is total over `u64` —
    /// the top octave (every value ≥ 2^63) was one `BUCKETS` short and
    /// indexed past the array (`the len is 1888 but the index is 1888`).
    #[test]
    fn record_is_total_over_u64() {
        let mut h = LogHistogram::new();
        for v in [0u64, 1, 31, 32, 1 << 62, (1 << 63) - 1, 1 << 63, u64::MAX] {
            h.record(v);
        }
        assert_eq!(h.count(), 8);
        assert_eq!(h.max(), u64::MAX);
        assert!(h.percentile(100.0) >= (1u64 << 63));
        assert_eq!(h.percentile(100.0), u64::MAX);
        // The top octave is real buckets, not a clamp: 2^63 and u64::MAX
        // are told apart (a clamp would flatter p99.9 — L10).
        let mut top = LogHistogram::new();
        top.record(1 << 63);
        assert!(top.percentile(100.0) < u64::MAX);
        assert_eq!(LogHistogram::index_of(u64::MAX), BUCKETS - 1);
        assert_eq!(LogHistogram::bucket_upper(BUCKETS - 1), u64::MAX);
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

    #[test]
    fn snapshot_copy_reuses_storage_and_keeps_bucket_bounds_total() {
        let mut source = LogHistogram::new();
        let mut snapshot = LogHistogram::new();
        let storage = snapshot.bucket_counts().as_ptr();
        for value in [0, 31, 32, 499, 500, 1000, 1 << 63, u64::MAX] {
            source.record(value);
            snapshot.copy_from(&source);
            assert_eq!(snapshot.bucket_counts().as_ptr(), storage);
            assert_eq!(snapshot.bucket_counts(), source.bucket_counts());
            assert_eq!(snapshot.count(), source.count());
            assert_eq!(snapshot.max(), source.max());
            assert_eq!(snapshot.sum(), source.sum());
            assert!(
                LogHistogram::bucket_upper_bound(LogHistogram::index_of(value)).unwrap() >= value
            );
        }
        assert_eq!(LogHistogram::bucket_upper_bound(LogHistogram::BUCKET_COUNT), None);
        assert_eq!(LogHistogram::bucket_upper_bound(usize::MAX), None);
    }
}
